//! Best-effort Windows toast notifications for runtime setup.

use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use crate::{DownloadProgress, RuntimeSetupProgress};

const TOAST_TAG: &str = "windows-app-sdk-runtime";
const TOAST_GROUP: &str = "runtime-setup";

type ToastNotifier = windows::UI::Notifications::ToastNotifier;

enum Notification {
    Downloading(DownloadProgress),
    Installing,
    Completed,
    Failed(String),
}

pub(crate) struct RuntimeNotifier {
    sender: Option<Sender<Notification>>,
    worker: Option<JoinHandle<()>>,
}

impl RuntimeNotifier {
    pub(crate) fn new(app_user_model_id: String, app_display_name: String) -> Self {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("wasdk-toast-notifications".to_string())
            .spawn(move || notification_thread(receiver, app_user_model_id, app_display_name))
            .ok();
        let sender = worker.as_ref().map(|_| sender);

        Self { sender, worker }
    }

    pub(crate) fn update(&self, progress: RuntimeSetupProgress) {
        let notification = match progress {
            RuntimeSetupProgress::Downloading(progress) => Notification::Downloading(progress),
            RuntimeSetupProgress::Installing => Notification::Installing,
            RuntimeSetupProgress::Installed => Notification::Completed,
            RuntimeSetupProgress::AlreadyInstalled | RuntimeSetupProgress::RuntimeMissing => return,
        };
        self.send(notification);
    }

    pub(crate) fn failed(&self, message: &str) {
        self.send(Notification::Failed(message.to_string()));
    }

    fn send(&self, notification: Notification) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(notification);
        }
    }
}

impl Drop for RuntimeNotifier {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn notification_thread(
    receiver: mpsc::Receiver<Notification>,
    app_user_model_id: String,
    app_display_name: String,
) {
    use windows::Win32::combaseapi::{CoInitializeEx, CoUninitialize};
    use windows::Win32::objbase::COINIT_MULTITHREADED;

    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED as u32).ok() } {
        eprintln!("Failed to initialize toast COM thread: {error}");
        return;
    }

    let (notifier, registration) =
        match initialize_toast_notifier(&app_user_model_id, &app_display_name) {
            Ok(initialized) => initialized,
            Err(error) => {
                eprintln!("Failed to initialize Windows toast notifications: {error}");
                unsafe { CoUninitialize() };
                return;
            }
        };

    let mut progress_active = false;
    let mut sequence = 0_u32;
    for notification in receiver {
        let result = match notification {
            notification @ (Notification::Downloading(_) | Notification::Installing) => {
                let next_sequence = sequence.saturating_add(1).max(1);
                let result = if progress_active {
                    update_progress(&notifier, &app_display_name, &notification, next_sequence)
                } else {
                    show_notification(&notifier, &app_display_name, &notification, next_sequence)
                };
                if result.is_ok() {
                    sequence = next_sequence;
                    progress_active = true;
                } else {
                    progress_active = false;
                }
                result
            }
            notification => {
                progress_active = false;
                sequence = 0;
                show_notification(&notifier, &app_display_name, &notification, 0)
            }
        };
        if let Err(error) = result {
            eprintln!("Failed to show Windows toast notification: {error}");
        }
    }

    drop(notifier);
    drop(registration);
    unsafe { CoUninitialize() };
}

fn initialize_toast_notifier(
    app_user_model_id: &str,
    app_display_name: &str,
) -> windows::core::Result<(ToastNotifier, ToastRegistration)> {
    use windows::UI::Notifications::ToastNotificationManager;
    use windows::core::HSTRING;

    let registration = register_notification_app(app_user_model_id, app_display_name)?;
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_user_model_id))?;
    Ok((notifier, registration))
}

fn register_notification_app(
    app_user_model_id: &str,
    app_display_name: &str,
) -> windows::core::Result<ToastRegistration> {
    use windows::Win32::winnt::{KEY_QUERY_VALUE, KEY_SET_VALUE, REG_CREATED_NEW_KEY, REG_SZ};
    use windows::Win32::winreg::{HKEY_CURRENT_USER, RegCloseKey, RegCreateKeyExW};
    use windows::core::PCWSTR;

    let path = wide(&format!(
        "Software\\Classes\\AppUserModelId\\{app_user_model_id}"
    ));
    let mut key = windows::Win32::HKEY::default();
    let mut disposition = 0;
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            PCWSTR::null(),
            0,
            (KEY_QUERY_VALUE | KEY_SET_VALUE) as u32,
            None,
            &mut key,
            Some(&mut disposition),
        )
    };
    if status != 0 {
        return Err(registry_error(
            status as u32,
            "Could not register toast app ID",
        ));
    }

    let created_key = disposition == REG_CREATED_NEW_KEY as u32;
    let previous_display_name = match query_registry_value(key, "DisplayName") {
        Ok(value) => value,
        Err(error) => {
            unsafe {
                let _ = RegCloseKey(key);
            }
            if created_key {
                delete_registry_key(&path);
            }
            return Err(error);
        }
    };
    let registered_display_name = RegistryValue {
        value_type: REG_SZ,
        data: app_display_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect(),
    };
    let mut registration = ToastRegistration {
        key,
        path,
        previous_display_name,
        registered_display_name,
        created_key,
        active: false,
    };
    if let Err(error) =
        set_registry_value(key, "DisplayName", &registration.registered_display_name)
    {
        return Err(error);
    }
    registration.active = true;
    Ok(registration)
}

#[derive(Eq, PartialEq)]
struct RegistryValue {
    value_type: u32,
    data: Vec<u8>,
}

struct ToastRegistration {
    key: windows::Win32::HKEY,
    path: Vec<u16>,
    previous_display_name: Option<RegistryValue>,
    registered_display_name: RegistryValue,
    created_key: bool,
    active: bool,
}

impl Drop for ToastRegistration {
    fn drop(&mut self) {
        use windows::Win32::winreg::{RegCloseKey, RegDeleteValueW};
        use windows::core::PCWSTR;

        let mut delete_created_key = self.created_key && !self.active;
        if self.active {
            match query_registry_value(self.key, "DisplayName") {
                Ok(Some(current)) if current == self.registered_display_name => {
                    match &self.previous_display_name {
                        Some(previous) => {
                            if let Err(error) =
                                set_registry_value(self.key, "DisplayName", previous)
                            {
                                eprintln!("Failed to restore toast app registration: {error}");
                            }
                        }
                        None => {
                            let name = wide("DisplayName");
                            let status = unsafe {
                                RegDeleteValueW(self.key, PCWSTR::from_raw(name.as_ptr()))
                            };
                            if status == 0 || status == 2 {
                                delete_created_key = self.created_key;
                            } else {
                                eprintln!(
                                    "Failed to remove toast app display name (Win32 error {status})"
                                );
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("Failed to inspect toast app registration during cleanup: {error}");
                }
            }
        }

        unsafe {
            let _ = RegCloseKey(self.key);
        }
        if delete_created_key {
            delete_registry_key(&self.path);
        }
    }
}

fn query_registry_value(
    key: windows::Win32::HKEY,
    name: &str,
) -> windows::core::Result<Option<RegistryValue>> {
    use windows::Win32::winreg::RegQueryValueExW;
    use windows::core::PCWSTR;

    let name = wide(name);
    let mut value_type = 0;
    let mut size = 0;
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut size),
        )
    };
    if status == 2 {
        return Ok(None);
    }
    if status != 0 {
        return Err(registry_error(
            status as u32,
            "Could not read toast app registration",
        ));
    }

    let mut data = vec![0; size as usize];
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(data.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status != 0 {
        return Err(registry_error(
            status as u32,
            "Could not read toast app registration",
        ));
    }
    data.truncate(size as usize);
    Ok(Some(RegistryValue { value_type, data }))
}

fn set_registry_value(
    key: windows::Win32::HKEY,
    name: &str,
    value: &RegistryValue,
) -> windows::core::Result<()> {
    use windows::Win32::winreg::RegSetValueExW;
    use windows::core::PCWSTR;

    let name = wide(name);
    let size = u32::try_from(value.data.len())
        .map_err(|_| notification_error("Toast registration value is too long"))?;
    let status = unsafe {
        RegSetValueExW(
            key,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            value.value_type,
            Some(value.data.as_ptr()),
            size,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(registry_error(
            status as u32,
            "Could not set toast app display name",
        ))
    }
}

fn delete_registry_key(path: &[u16]) {
    use windows::Win32::winreg::{HKEY_CURRENT_USER, RegDeleteKeyW};
    use windows::core::PCWSTR;

    let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR::from_raw(path.as_ptr())) };
    if status != 0 && status != 2 {
        eprintln!("Failed to remove toast app registration key (Win32 error {status})");
    }
}

fn registry_error(status: u32, message: &str) -> windows::core::Error {
    use windows::core::{Error, WIN32_ERROR};

    Error::new(WIN32_ERROR(status).to_hresult(), message)
}

fn show_notification(
    notifier: &ToastNotifier,
    app_display_name: &str,
    notification: &Notification,
    sequence: u32,
) -> windows::core::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::ToastNotification;
    use windows::core::HSTRING;

    let (message, has_progress) = match notification {
        Notification::Downloading(_) => ("正在下载 Windows App SDK 运行时".to_string(), true),
        Notification::Installing => ("正在安装 Windows App SDK 运行时".to_string(), true),
        Notification::Completed => ("Windows App SDK 运行时安装完成".to_string(), false),
        Notification::Failed(error) => (format!("运行时安装失败：{error}"), false),
    };
    let progress_xml = if has_progress {
        "<progress value=\"{progressValue}\" status=\"{progressStatus}\" valueStringOverride=\"{progressValueString}\" />"
    } else {
        ""
    };
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text id=\"1\">{}</text>{}</binding></visual></toast>",
        xml_escape(app_display_name),
        xml_escape(&message),
        progress_xml,
    );

    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    if has_progress {
        toast.SetData(&progress_data(notification, sequence)?)?;
    }
    let _ = toast.SetTag(&HSTRING::from(TOAST_TAG));
    let _ = toast.SetGroup(&HSTRING::from(TOAST_GROUP));
    notifier.Show(&toast)
}

fn update_progress(
    notifier: &ToastNotifier,
    app_display_name: &str,
    notification: &Notification,
    sequence: u32,
) -> windows::core::Result<()> {
    use windows::UI::Notifications::NotificationUpdateResult;
    use windows::core::{Error, HRESULT, HSTRING};

    let result = notifier.UpdateWithTagAndGroup(
        &progress_data(notification, sequence)?,
        &HSTRING::from(TOAST_TAG),
        &HSTRING::from(TOAST_GROUP),
    )?;
    match result {
        NotificationUpdateResult::Succeeded => Ok(()),
        NotificationUpdateResult::NotificationNotFound => {
            show_notification(notifier, app_display_name, notification, sequence)
        }
        _ => Err(Error::new(
            HRESULT(0x8000_4005_u32 as i32),
            "Toast progress update failed",
        )),
    }
}

fn progress_data(
    notification: &Notification,
    sequence: u32,
) -> windows::core::Result<windows::UI::Notifications::NotificationData> {
    use windows::UI::Notifications::NotificationData;
    use windows::core::HSTRING;

    let (message, value, value_string, status) = match notification {
        Notification::Downloading(DownloadProgress {
            received_bytes,
            total_bytes: Some(total),
        }) if *total > 0 => {
            let value = (*received_bytes as f64 / *total as f64).clamp(0.0, 1.0);
            (
                "正在下载 Windows App SDK 运行时",
                format!("{value:.4}"),
                format!("{}%", (value * 100.0).round() as u64),
                "正在下载",
            )
        }
        Notification::Downloading(_) => (
            "正在下载 Windows App SDK 运行时",
            "indeterminate".to_string(),
            String::new(),
            "正在下载",
        ),
        Notification::Installing => (
            "正在安装 Windows App SDK 运行时",
            "indeterminate".to_string(),
            String::new(),
            "正在安装",
        ),
        _ => return Err(notification_error("Notification has no progress data")),
    };

    let data = NotificationData::new()?;
    data.SetSequenceNumber(sequence)?;
    let values = data.Values()?;
    values.Insert(&HSTRING::from("1"), &HSTRING::from(message))?;
    values.Insert(&HSTRING::from("progressValue"), &HSTRING::from(value))?;
    values.Insert(
        &HSTRING::from("progressValueString"),
        &HSTRING::from(value_string),
    )?;
    values.Insert(&HSTRING::from("progressStatus"), &HSTRING::from(status))?;
    Ok(data)
}

fn notification_error(message: impl std::fmt::Display) -> windows::core::Error {
    windows::core::Error::new(
        windows::core::HRESULT(0x8000_4005_u32 as i32),
        message.to_string(),
    )
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
