//! This is the common code for creating a camera instance
//!
//! Features:
//!    Shared stream BC delivery
//!    Common restart code
//!    Clonable interface to share amongst threadsanyhow::anyhow;
use futures::{stream::StreamExt, TryFutureExt};
use std::sync::Weak;
use tokio::{
    sync::{
        mpsc::{channel as mpsc, Sender as MpscSender},
        oneshot::{channel as oneshot, Sender as OneshotSender},
        watch::{channel as watch, Receiver as WatchReceiver, Sender as WatchSender},
    },
    task::JoinSet,
    time::{sleep, Duration},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use super::{
    MdRequest, MdState, NeoCamMdThread, NeoCamStreamThread, NeoCamThread, NeoCamThreadState,
    NeoInstance, Permit, PnRequest, PushNoti, PushNotiThread, StreamInstance, StreamRequest,
    UseCounter,
};
use crate::{config::CameraConfig, AnyResult, Result};
use neolink_core::bc_protocol::{BcCamera, StreamKind};

#[allow(dead_code)]
pub(crate) enum NeoCamCommand {
    HangUp,
    Instance(OneshotSender<Result<NeoInstance>>),
    Stream(StreamKind, OneshotSender<StreamInstance>),
    HighStream(OneshotSender<Option<StreamInstance>>),
    LowStream(OneshotSender<Option<StreamInstance>>),
    Streams(OneshotSender<Vec<StreamInstance>>),
    Motion(OneshotSender<WatchReceiver<MdState>>),
    Config(OneshotSender<WatchReceiver<CameraConfig>>),
    Disconnect(OneshotSender<()>),
    Connect(OneshotSender<()>),
    State(OneshotSender<NeoCamThreadState>),
    GetPermit(OneshotSender<Permit>),
    PushNoti(OneshotSender<WatchReceiver<Option<PushNoti>>>),
}
/// The underlying camera binding
pub(crate) struct NeoCam {
    cancel: CancellationToken,
    config_watch: WatchSender<CameraConfig>,
    commander: MpscSender<NeoCamCommand>,
    camera_watch: WatchReceiver<Weak<BcCamera>>,
    set: JoinSet<AnyResult<()>>,
}

impl NeoCam {
    pub(crate) async fn new(config: CameraConfig) -> Result<NeoCam> {
        let (commander_tx, commander_rx) = mpsc(100);
        let (watch_config_tx, watch_config_rx) = watch(config.clone());
        let (camera_watch_tx, camera_watch_rx) = watch(Weak::new());
        let (stream_request_tx, stream_request_rx) = mpsc(100);
        let (md_request_tx, md_request_rx) = mpsc(100);
        let (pn_request_tx, mut pn_request_rx) = mpsc(100);
        let (state_tx, state_rx) = watch(NeoCamThreadState::Connected);

        let set = JoinSet::new();
        let users = UseCounter::new().await;

        let mut me = Self {
            cancel: CancellationToken::new(),
            config_watch: watch_config_tx,
            commander: commander_tx.clone(),
            camera_watch: camera_watch_rx.clone(),
            set,
        };

        // This thread recieves messages from the instances
        // and acts on it.
        //
        // This thread must be started first so that we can begin creating instances for the
        // other threads
        let sender_cancel = me.cancel.clone();
        let mut commander_rx = ReceiverStream::new(commander_rx);
        let strict = config.strict;
        let thread_commander_tx = commander_tx.clone();
        let thread_watch_config_rx = watch_config_rx.clone();
        me.set.spawn(async move {
            let thread_cancel = sender_cancel.clone();
            let res = tokio::select! {
                _ = sender_cancel.cancelled() => {
                    log::debug!("Control thread Cancelled");
                    Result::Ok(())
                },
                v = async {
                    while let Some(command) = commander_rx.next().await {
                        match command {
                            NeoCamCommand::HangUp => {
                                log::debug!("Cancel:: NeoCamCommand::HangUp");
                                sender_cancel.cancel();
                                return Result::<(), anyhow::Error>::Ok(());
                            }
                            NeoCamCommand::Instance(result) => {
                                let instance = NeoInstance::new(
                                    camera_watch_rx.clone(),
                                    thread_commander_tx.clone(),
                                    thread_cancel.clone(),
                                );
                                let _ = result.send(instance);
                            }
                            NeoCamCommand::Stream(name, sender) => {
                                stream_request_tx.send(
                                    StreamRequest::GetOrInsert {
                                        name,
                                        sender,
                                        strict,
                                    }
                                ).await?;
                            },
                            NeoCamCommand::HighStream(sender) => {
                                stream_request_tx.send(
                                    StreamRequest::High {
                                        sender,
                                    }
                                ).await?;
                            },
                            NeoCamCommand::LowStream(sender) => {
                                stream_request_tx.send(
                                    StreamRequest::Low {
                                        sender,
                                    }
                                ).await?;
                            },
                            NeoCamCommand::Streams(sender) => {
                                stream_request_tx.send(
                                    StreamRequest::All {
                                        sender,
                                    }
                                ).await?;
                            },
                            NeoCamCommand::Motion(sender) => {
                                md_request_tx.send(
                                    MdRequest::Get {
                                        sender,
                                    }
                                ).await?;
                            },
                            NeoCamCommand::Config(sender) => {
                                let _ = sender.send(thread_watch_config_rx.clone());
                            },
                            NeoCamCommand::Connect(sender) => {
                                if !matches!(*state_tx.borrow(), NeoCamThreadState::Connected) {
                                    state_tx.send_replace(NeoCamThreadState::Connected);
                                    log::debug!("{}: Connect On Request", thread_watch_config_rx.borrow().name);
                                }
                                let _ = sender.send(());
                            }
                            NeoCamCommand::Disconnect(sender) => {
                                if !matches!(*state_tx.borrow(), NeoCamThreadState::Disconnected) {
                                    state_tx.send_replace(NeoCamThreadState::Disconnected);
                                    log::debug!("{}: Disconnect On Request", thread_watch_config_rx.borrow().name);
                                }
                                let _ = sender.send(());
                            }
                            NeoCamCommand::State(sender) => {
                                let _ = sender.send(*state_tx.borrow());
                            }
                            NeoCamCommand::GetPermit(sender) => {
                                let _ = sender.send(users.create_activated().await?);
                            }
                            NeoCamCommand::PushNoti(sender) => {
                                pn_request_tx.send(
                                    PnRequest::Get {
                                        sender,
                                    }
                                ).await?;
                            },
                        }
                    }
                    log::debug!("Control thread Senders dropped");
                    Ok(())
                } => v
            };
            log::debug!("Control thread terminated");
            res
        });

        // This gets the first instance which we use for making the other threads
        let (instance_tx, instance_rx) = oneshot();
        commander_tx
            .send(NeoCamCommand::Instance(instance_tx))
            .await?;
        let instance = instance_rx.await??;

        // This thread maintains the camera loop
        //
        // It will keep it logged and reconnect
        let thread_watch_config_rx = watch_config_rx.clone();
        let mut cam_thread = NeoCamThread::new(
            state_rx,
            thread_watch_config_rx,
            camera_watch_tx,
            me.cancel.clone(),
        )
        .await;
        me.set.spawn(async move { cam_thread.run().await });

        // This thread maintains the streams
        let stream_instance = instance.subscribe().await?;
        let stream_cancel = me.cancel.clone();
        let mut stream_thread = NeoCamStreamThread::new(stream_request_rx, stream_instance).await?;
        me.set.spawn(async move {
            tokio::select! {
                _ = stream_cancel.cancelled() => AnyResult::Ok(()),
                v = stream_thread.run() => v,
            }
        });

        // This thread monitors the motion
        let md_instance = instance.subscribe().await?;
        let md_cancel = me.cancel.clone();
        let mut md_thread = NeoCamMdThread::new(md_request_rx, md_instance).await?;
        me.set.spawn(async move {
            tokio::select! {
                _ = md_cancel.cancelled() => AnyResult::Ok(()),
                v = md_thread.run() => v,
            }
        });

        // This thread just does a one time report on camera info
        let report_instance = instance.subscribe().await?;
        let report_cancel = me.cancel.clone();
        let report_name = config.name.clone();
        me.set.spawn(async move {
            tokio::select! {
                _ = report_cancel.cancelled() => {
                    AnyResult::Ok(())
                }
                v = async {
                    let version = report_instance.run_task(|cam| Box::pin(
                        async move {
                            Ok(cam.version().await?)
                        }
                    )).await?;
                    log::info!("{}: Model {}", report_name, version.model.unwrap_or("Undeclared".to_string()));
                    log::info!("{}: Firmware Version {}", report_name, version.firmwareVersion);

                    let stream_info = report_instance.run_task(|cam| Box::pin(
                        async move {
                            Ok(cam.get_stream_info().await?)
                        }
                    )).await?;
                    let mut supported_streams = vec![];
                    for encode in stream_info.stream_infos.iter().flat_map(|stream_info| stream_info.encode_tables.clone()) {
                        supported_streams.push(std::format!("    {}: {}x{}", encode.name, encode.resolution.width, encode.resolution.height));
                    }
                    log::debug!("{}: Listing Camera Supported Streams\n{}", report_name, supported_streams.join("\n"));


                    Ok(())
                } => v
            }
        });

        // Handles push notifications
        let pn_root_instance = instance.subscribe().await?;
        let pn_cancel = me.cancel.clone();
        me.set.spawn(async move {
            tokio::select!{
                _ = pn_cancel.cancelled() => {
                    AnyResult::Ok(())
                },
                v = async {
                    let mut config_rx = pn_root_instance.config().await?;
                    loop {
                        // Wait for the green light
                        config_rx.wait_for(|config| config.push_notifications).await?;
                        let pn_instance = pn_root_instance.subscribe().await?;
                        let mut push_notifier = PushNotiThread::new(pn_instance).await?;

                        let pn_permit_instance = pn_root_instance.subscribe().await?;
                        let r = tokio::select! {
                            // This thread handles the push notfications
                            v = push_notifier.run(&mut pn_request_rx) => v,
                            // Push notification permits
                            v = async {
                                let mut prev_noti = None;
                                let mut pn = pn_permit_instance.push_notifications().await?;
                                loop{
                                    prev_noti = pn.wait_for(|noti| noti != &prev_noti && noti.is_some()).await.map(|noti| noti.clone())?;
                                    let _permit = pn_permit_instance.permit().await?;
                                    sleep(Duration::from_secs(30)).await; // Push notification will wake us up for 30s
                                }
                            } => v,
                            // Continue loop on Red light
                            v = config_rx.wait_for(|config| !config.push_notifications).map_ok(|_| ()) => {
                                v?;
                                AnyResult::Ok(())
                            },
                        };
                        if r.is_err() {
                            log::debug!("Push notifications stopped: {:?}", r);
                            break r;
                        }
                    }?;
                    AnyResult::Ok(())
                } => v,
            }
        });

        // MD permits
        let md_permit_instance = instance.subscribe().await?;
        let md_permit_cancel = me.cancel.clone();
        me.set.spawn(async move {
            tokio::select! {
                _ = md_permit_cancel.cancelled() => {
                    AnyResult::Ok(())
                },
                v = async {
                    let mut md = md_permit_instance.motion().await?;
                    loop{
                        md.wait_for(|md| matches!(md, MdState::Start(_))).await?;
                        let _permit = Some(md_permit_instance.permit().await?);
                        md.wait_for(|md| matches!(md, MdState::Stop(_))).await?;
                        // Try waiting for 30s
                        // If in those 30s we get motion then return to
                        // loop early to reaquire the permit
                        tokio::select!{
                            _ = sleep(Duration::from_secs(30)) => {},
                            _ = md.wait_for(|md| matches!(md, MdState::Start(_))) => {},
                        }
                    }
                } => v,
            }
        });

        // This thread will apply battery saving by disconnecting the camera when there are no
        // active permits.
        //
        // Permits are created when a camera runs a user requested task, or when motion or push
        // notifications are observed
        let connect_instance = instance.subscribe().await?;
        let connect_cancel = me.cancel.clone();
        let connect_name = config.name.clone();
        me.set.spawn(async move {
            tokio::select!{
                _ = connect_cancel.cancelled() => {
                    AnyResult::Ok(())
                },
                v = async {
                    let mut config_rx = connect_instance.config().await?;
                    loop {
                        // Wait for the green light
                        config_rx.wait_for(|config| config.idle_disconnect).await?;

                        let r = tokio::select!{
                            // Wait for red light
                            v = config_rx.wait_for(|config| !config.idle_disconnect).map_ok(|_| ()) => {
                                v?;
                                connect_instance.connect().await?; // Ensure we are online now that we are not idle_disconnect
                                AnyResult::Ok(())
                            }
                            // Handle disconnects when no active permits
                            v = async {
                                let mut permit = connect_instance.permit().await?;
                                permit.deactivate().await?; // Watching only from here
                                loop {
                                    permit.aquired_users().await?;
                                    log::debug!("{connect_name}: InUse");
                                    connect_instance.connect().await?;
                                    permit.dropped_users().await?;
                                    log::debug!("{connect_name}: Idle Wait");
                                    // Wait 30s or if we hit another use then go back and wait again
                                    tokio::select! {
                                        _ = sleep(Duration::from_secs(30)) => {},
                                        _ = permit.aquired_users() => continue,
                                    };
                                    log::debug!("{connect_name}: Idle");
                                    connect_instance.disconnect().await?;
                                }
                            } => v,
                        };
                        if r.is_err() {
                            break r;
                        }
                    }?;
                    AnyResult::Ok(())
                } => v,
            }
        });

        Ok(me)
    }

    pub(crate) async fn subscribe(&self) -> Result<NeoInstance> {
        NeoInstance::new(
            self.camera_watch.clone(),
            self.commander.clone(),
            self.cancel.clone(),
        )
    }

    pub(crate) async fn update_config(&self, config: CameraConfig) -> Result<()> {
        self.config_watch.send_replace(config);
        Ok(())
    }
}

impl Drop for NeoCam {
    fn drop(&mut self) {
        log::trace!("Drop NeoCam");
        let mut set = std::mem::take(&mut self.set);
        let commander = self.commander.clone();
        let _gt = tokio::runtime::Handle::current().enter();
        tokio::task::spawn(async move {
            let _ = commander.send(NeoCamCommand::HangUp).await;
            while set.join_next().await.is_some() {}
            log::trace!("Dropped NeoCam");
        });
    }
}
