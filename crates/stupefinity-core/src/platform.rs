use tracing::{info, warn};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, CreateMutexW, GetCurrentProcess, ProcessPowerThrottling,
    SetProcessInformation, SetProcessWorkingSetSize, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE,
};

pub struct PlatformManager {
    #[cfg(windows)]
    mutex_handle: Option<HANDLE>,
}

impl Default for PlatformManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformManager {
    pub fn new() -> Self {
        Self {
            #[cfg(windows)]
            mutex_handle: None,
        }
    }

    /// Enforces a single instance of Stupefinity Core via a Windows Named Mutex.
    /// Returns false if another instance is already running.
    pub fn enforce_single_instance(&mut self) -> bool {
        #[cfg(windows)]
        unsafe {
            let mutex_name: Vec<u16> = "Global\\StupefinityCoreNamedMutex\0"
                .encode_utf16()
                .collect();
            let handle = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
            if handle.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
                return false;
            }
            self.mutex_handle = Some(handle);
        }
        true
    }

    /// Registers the calling thread with the Multimedia Class Scheduler Service (MMCSS)
    /// "Audio" task so it gets glitch-resistant scheduling priority.
    /// Must be called from the audio thread itself.
    pub fn enable_mmcss_audio() {
        #[cfg(windows)]
        unsafe {
            let task: Vec<u16> = "Audio\0".encode_utf16().collect();
            let mut task_index = 0u32;
            // The returned handle is intentionally never reverted: the registration
            // lives exactly as long as the audio thread.
            if AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_index).is_null() {
                warn!("MMCSS registration failed (error {}).", GetLastError());
            } else {
                info!("Audio thread registered with MMCSS 'Audio' task.");
            }
        }
    }

    /// Enables or disables Windows 11 EcoQoS (Power Throttling).
    /// When the window is hidden, EcoQoS directs background execution to efficiency cores.
    pub fn set_eco_qos(enable: bool) {
        #[cfg(windows)]
        unsafe {
            let mut state = PROCESS_POWER_THROTTLING_STATE {
                Version: 1,
                ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                StateMask: if enable {
                    PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                } else {
                    0
                },
            };

            let res = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &mut state as *mut _ as *mut _,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );

            if res != 0 {
                info!("Windows 11 EcoQoS state set to: {}", enable);
            }
        }
    }

    /// Trims the process working set, releasing physical memory back to the OS.
    /// Called when the application window is minimized or hidden to the system tray.
    pub fn trim_working_set() {
        #[cfg(windows)]
        unsafe {
            let res = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
            if res != 0 {
                info!("Trimmed working set to minimum resident pages.");
            }
        }
    }
}
