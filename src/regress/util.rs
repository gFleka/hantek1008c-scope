//! Shared helpers for the regression suite: signal builders, temp folders, waiting, config isolation.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Piecewise-constant signal: `[(samples, value), …]`.
pub fn seg(parts: &[(usize, u16)]) -> Vec<u16> {
    parts
        .iter()
        .flat_map(|&(n, v)| std::iter::repeat_n(v, n))
        .collect()
}

/// Deterministic pseudo-random numbers in -1…1 (LCG; no dependency, same on every run).
pub struct Lcg(pub u64);
impl Lcg {
    pub fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }
}

/// A fresh, empty temp folder unique to this test (removed first if left over).
pub fn tmp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("h8-regress-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Poll `cond` every 10 ms until it holds or `secs` pass. Returns whether it held.
pub fn wait_until(secs: f32, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f32() < secs {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

/// Config isolation for everything that reads the config / home folder (settings.txt, files.txt,
/// calibration.txt, references, the default recording folder). Holding the guard: HOME and
/// XDG_CONFIG_HOME point into a fresh temp folder and no other isolated test runs. The owner's
/// ~/.config and ~/Documents are never touched.
pub struct Isolated {
    pub dir: PathBuf,
    _g: MutexGuard<'static, ()>,
}

static ENV_LOCK: Mutex<()> = Mutex::new(());
static REAL_CONFIG: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// The owner's real config folder (before any isolation), to check it is never written.
pub fn real_config_dir() -> Option<PathBuf> {
    REAL_CONFIG.get_or_init(crate::hantek::config_dir).clone()
}

pub fn isolated(name: &str) -> Isolated {
    let g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    real_config_dir();
    let dir = tmp_dir(&format!("cfg-{name}"));
    std::fs::create_dir_all(dir.join("config")).unwrap();
    // SAFETY-ish: tests that read these variables all hold ENV_LOCK; std's env access is locked internally
    std::env::remove_var("APPDATA");
    std::env::remove_var("USERPROFILE");
    std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
    std::env::set_var("HOME", &dir);
    assert_eq!(
        crate::hantek::config_dir().unwrap(),
        dir.join("config").join("hantek1008c")
    );
    Isolated { dir, _g: g }
}

impl Isolated {
    /// The app's config folder inside the isolated home.
    pub fn cfg(&self) -> PathBuf {
        self.dir.join("config").join("hantek1008c")
    }
    pub fn write(&self, file: &str, txt: &str) {
        std::fs::create_dir_all(self.cfg()).unwrap();
        std::fs::write(self.cfg().join(file), txt).unwrap();
    }
}

/// Counting allocator (test binary only): allocations and bytes since start, for per-frame allocation numbers in
/// the speed benches. Two relaxed atomic adds per allocation.
pub mod alloc_count {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    pub struct Counting;
    pub static N: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        // const init, no destructor: safe to touch from inside the allocator
        static T_N: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        static T_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    fn count(bytes: usize) {
        N.fetch_add(1, Relaxed);
        BYTES.fetch_add(bytes as u64, Relaxed);
        let _ = T_N.try_with(|c| c.set(c.get() + 1));
        let _ = T_BYTES.try_with(|c| c.set(c.get() + bytes as u64));
    }
    /// (allocations, bytes) of the calling thread so far.
    pub fn this_thread() -> (u64, u64) {
        (T_N.with(|c| c.get()), T_BYTES.with(|c| c.get()))
    }
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            count(l.size());
            System.alloc(l)
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            System.dealloc(p, l)
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            count(n);
            System.realloc(p, l, n)
        }
    }
    #[global_allocator]
    static A: Counting = Counting;
    /// (allocations, bytes) so far (all threads).
    pub fn now() -> (u64, u64) {
        (N.load(Relaxed), BYTES.load(Relaxed))
    }
}
