use windows::Win32::System::Diagnostics::Debug::Beep;

pub fn play_start(enabled: bool) {
    if !enabled {
        return;
    }
    std::thread::spawn(|| unsafe {
        // Short rising chirp: 880 Hz for 60 ms.
        let _ = Beep(880, 60);
    });
}

pub fn play_stop(enabled: bool) {
    if !enabled {
        return;
    }
    std::thread::spawn(|| unsafe {
        // Short falling chirp: 660 Hz for 60 ms.
        let _ = Beep(660, 60);
    });
}
