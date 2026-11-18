use super::{
    CONFIRM_LABEL, Confirmation, OPEN_LABEL, RETRY_INTERVAL, STOP_LABEL, icon, open_app,
    request_stop,
};
use anyhow::Result;
use ksni::{Tray, TrayMethods, menu::StandardItem};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn is_container() -> bool {
    std::env::var_os("container").is_some_and(|value| !value.is_empty())
        || [
            "/.dockerenv",
            "/run/.containerenv",
            "/run/systemd/container",
            "/run/host/container-manager",
            "/run/.toolboxenv",
            "/.flatpak-info",
        ]
        .iter()
        .any(|path| Path::new(path).exists())
}

pub(super) async fn watcher_available() -> bool {
    let probe = async {
        let bus = zbus::Connection::session().await?;
        zbus::fdo::DBusProxy::new(&bus)
            .await?
            .name_has_owner(WATCHER.try_into().map_err(zbus::Error::from)?)
            .await
    };
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, probe).await,
        Ok(Ok(true))
    )
}

enum Action {
    Open,
    Stop,
    Confirm,
}

struct Notifier {
    id: String,
    title: String,
    image: ksni::Icon,
    stop_armed: bool,
    actions: mpsc::UnboundedSender<Action>,
    offline: CancellationToken,
}

impl Tray for Notifier {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title(),
            ..Default::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.image.clone()]
    }

    fn watcher_offline(&self, _: ksni::OfflineReason) -> bool {
        self.offline.cancel();
        false
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.title.replace('_', "__"),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: OPEN_LABEL.into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.actions.send(Action::Open);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: STOP_LABEL.into(),
                enabled: !self.stop_armed,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.actions.send(Action::Stop);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: CONFIRM_LABEL.into(),
                visible: self.stop_armed,
                enabled: self.stop_armed,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.actions.send(Action::Confirm);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub(super) async fn run(home: &Path, title: String, cancel: CancellationToken) -> Result<()> {
    let (width, height, mut data) = icon()?;
    for pixel in data.chunks_exact_mut(4) {
        pixel.rotate_right(1); // StatusNotifierItem uses network-order ARGB32.
    }
    let (actions, mut requests) = mpsc::unbounded_channel();
    let offline = CancellationToken::new();
    let notify = Notifier {
        id: format!(
            "sh.paseo.daemon.{:x}",
            Sha256::digest(home.as_os_str().as_encoded_bytes())
        ),
        title,
        image: ksni::Icon {
            width: width.try_into()?,
            height: height.try_into()?,
            data,
        },
        stop_armed: false,
        actions,
        offline: offline.clone(),
    };
    let tray = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = tokio::time::timeout(PROBE_TIMEOUT, notify.spawn()) => result??,
    };
    let mut confirmation = Confirmation::default();
    // Watcher shutdown signals cover shell restarts; a failed bus connection
    // may instead close the signal stream without delivering such a signal.
    let mut monitor =
        tokio::time::interval_at(tokio::time::Instant::now() + RETRY_INTERVAL, RETRY_INTERVAL);
    loop {
        let deadline = confirmation.0;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = offline.cancelled() => break,
            action = requests.recv() => match action {
                Some(Action::Open) => {
                    let home = home.to_owned();
                    tokio::spawn(async move { open_app(&home).await });
                }
                Some(Action::Confirm) if confirmation.confirmed() => {
                    request_stop()?;
                    break;
                }
                Some(Action::Stop) if confirmation.0.is_none() => {
                    confirmation.arm();
                    tray.update(|tray| tray.stop_armed = true).await;
                }
                Some(_) => {},
                None => break,
            },
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                if confirmation.expire() {
                    tray.update(|tray| tray.stop_armed = false).await;
                }
            }
            _ = monitor.tick() => {
                if !watcher_available().await {
                    break;
                }
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_millis(500), tray.shutdown()).await;
    Ok(())
}
