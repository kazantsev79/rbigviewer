//! Чтение файла блоками с LRU-кэшем и фоновым потоком загрузки.
//!
//! Интерфейс никогда не читает диск сам: он спрашивает блок в кэше,
//! а если блока нет — ставит его в очередь и рисует заглушку.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// Размер блока чтения.
pub const BLOCK: u64 = 1 << 20;
/// Сколько блоков держим в памяти (64 МБ на вкладку).
const CACHE_BLOCKS: usize = 64;
/// Максимальная длина очереди запросов: старые запросы выбрасываются,
/// чтобы при быстром перетаскивании ползунка не читать устаревшие места.
const MAX_QUEUE: usize = 16;

pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// Чтение по смещению без изменения общей позиции файла (pread / ReadFile с OVERLAPPED).
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(file, &mut buf[done..], offset + done as u64)?;
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(file, &mut buf[done..], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

/// Открыть файл так, чтобы другие программы могли продолжать в него писать,
/// переименовывать и удалять его (ротация логов).
/// В Rust на Windows это поведение по умолчанию: FILE_SHARE_READ | WRITE | DELETE.
pub fn open_shared(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[derive(Default)]
struct Cache {
    map: HashMap<u64, (Arc<Vec<u8>>, u64)>,
    tick: u64,
}

#[derive(Default)]
struct Queue {
    order: VecDeque<u64>,
    pending: HashSet<u64>,
    stop: bool,
}

struct Shared {
    file: File,
    size: AtomicU64,
    cache: Mutex<Cache>,
    queue: Mutex<Queue>,
    cv: Condvar,
    repaint: Repaint,
}

pub struct Source {
    shared: Arc<Shared>,
}

impl Source {
    pub fn open(path: &Path, repaint: Repaint) -> io::Result<Self> {
        let file = open_shared(path)?;
        let size = file.metadata()?.len();
        let shared = Arc::new(Shared {
            file,
            size: AtomicU64::new(size),
            cache: Mutex::default(),
            queue: Mutex::default(),
            cv: Condvar::new(),
            repaint,
        });
        let worker = shared.clone();
        thread::Builder::new()
            .name("block-loader".into())
            .spawn(move || loader(worker))?;
        Ok(Self { shared })
    }

    pub fn size(&self) -> u64 {
        self.shared.size.load(Ordering::Relaxed)
    }

    pub fn file(&self) -> &File {
        &self.shared.file
    }

    /// Перечитать размер файла. Возвращает (старый, новый).
    pub fn refresh_size(&self) -> (u64, u64) {
        let old = self.size();
        let new = self.shared.file.metadata().map(|m| m.len()).unwrap_or(old);
        if new != old {
            self.shared.size.store(new, Ordering::Relaxed);
            let mut cache = self.shared.cache.lock().unwrap();
            if new < old {
                // Файл обрезали — кэшу верить нельзя.
                cache.map.clear();
            } else {
                // Последний неполный блок вырос — его надо перечитать.
                cache.map.remove(&(old / BLOCK));
            }
        }
        (old, new)
    }

    /// Блок из кэша (без обращения к диску).
    pub fn get(&self, block: u64) -> Option<Arc<Vec<u8>>> {
        let mut cache = self.shared.cache.lock().unwrap();
        cache.tick += 1;
        let tick = cache.tick;
        cache.map.get_mut(&block).map(|(data, used)| {
            *used = tick;
            data.clone()
        })
    }

    /// Поставить блок в очередь на чтение, если его ещё нет.
    pub fn request(&self, block: u64) {
        if block * BLOCK >= self.size().max(1) {
            return;
        }
        if self.shared.cache.lock().unwrap().map.contains_key(&block) {
            return;
        }
        let mut q = self.shared.queue.lock().unwrap();
        if !q.pending.insert(block) {
            return;
        }
        q.order.push_back(block);
        while q.order.len() > MAX_QUEUE {
            if let Some(old) = q.order.pop_front() {
                q.pending.remove(&old);
            }
        }
        self.shared.cv.notify_one();
    }

    /// Байты [start, start+len), обрезанные по концу файла.
    /// Если каких-то блоков нет в кэше — запрашивает их и возвращает Err.
    pub fn read_span(&self, start: u64, len: u64) -> Result<Vec<u8>, Missing> {
        let size = self.size();
        let end = (start + len).min(size);
        if start >= end {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end - start) as usize);
        let mut missing = false;
        for b in start / BLOCK..=(end - 1) / BLOCK {
            let bstart = b * BLOCK;
            let from = start.max(bstart) - bstart;
            let to = end.min(bstart + BLOCK) - bstart;
            match self.get(b) {
                Some(data) if data.len() as u64 >= to => {
                    if !missing {
                        out.extend_from_slice(&data[from as usize..to as usize]);
                    }
                }
                _ => {
                    missing = true;
                    self.request(b);
                }
            }
        }
        if missing { Err(Missing) } else { Ok(out) }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.shared.queue.lock().unwrap().stop = true;
        self.shared.cv.notify_all();
    }
}

/// Нужных данных пока нет в памяти; они уже запрошены.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Missing;

fn loader(sh: Arc<Shared>) {
    let mut buf = vec![0u8; BLOCK as usize];
    loop {
        let block = {
            let mut q = sh.queue.lock().unwrap();
            loop {
                if q.stop {
                    return;
                }
                // Берём самый свежий запрос: он ближе всего к тому, что сейчас на экране.
                if let Some(b) = q.order.pop_back() {
                    break b;
                }
                q = sh.cv.wait(q).unwrap();
            }
        };
        let size = sh.size.load(Ordering::Relaxed);
        let start = block * BLOCK;
        let want = BLOCK.min(size.saturating_sub(start)) as usize;
        let data = match read_at(&sh.file, &mut buf[..want], start) {
            Ok(n) => buf[..n].to_vec(),
            Err(_) => Vec::new(),
        };
        {
            let mut cache = sh.cache.lock().unwrap();
            cache.tick += 1;
            let tick = cache.tick;
            cache.map.insert(block, (Arc::new(data), tick));
            if cache.map.len() > CACHE_BLOCKS {
                if let Some(&oldest) = cache.map.iter().min_by_key(|(_, (_, used))| *used).map(|(k, _)| k) {
                    cache.map.remove(&oldest);
                }
            }
        }
        sh.queue.lock().unwrap().pending.remove(&block);
        (sh.repaint)();
    }
}
