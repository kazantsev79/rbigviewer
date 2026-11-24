//! Фоновый разреженный индекс строк: для каждого блока в 1 МБ храним,
//! сколько переводов строк было до его начала. Для 10 ГБ это ~10 000 чисел.
//! Индексатор не останавливается в конце файла: если лог растёт, он дочитывает новое.

use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::encoding::Nl;
use crate::source::{BLOCK, Repaint, read_at};

#[derive(Default, Clone)]
pub struct IndexData {
    /// checkpoints[i] = число переводов строк до начала блока i.
    /// Длина = число полностью проиндексированных блоков + 1.
    pub checkpoints: Vec<u64>,
    /// Переводы строк в последнем, неполном блоке.
    pub tail_newlines: u64,
    /// Сколько байт файла учтено.
    pub indexed: u64,
    /// Файл заканчивается переводом строки.
    pub ends_with_nl: bool,
    /// Индекс дошёл до конца файла (на момент последней проверки).
    pub complete: bool,
}

impl IndexData {
    pub fn total_newlines(&self) -> u64 {
        self.checkpoints.last().copied().unwrap_or(0) + self.tail_newlines
    }
}

pub struct Index {
    pub data: Arc<Mutex<IndexData>>,
    cancel: Arc<AtomicBool>,
}

impl Index {
    pub fn start(file: File, nl: Nl, repaint: Repaint) -> Self {
        let data = Arc::new(Mutex::new(IndexData { checkpoints: vec![0], ..Default::default() }));
        let cancel = Arc::new(AtomicBool::new(false));
        let (d, c) = (data.clone(), cancel.clone());
        thread::Builder::new()
            .name("line-indexer".into())
            .spawn(move || run(file, nl, d, c, repaint))
            .expect("spawn indexer");
        Self { data, cancel }
    }

    pub fn snapshot(&self) -> IndexData {
        self.data.lock().unwrap().clone()
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn run(file: File, nl: Nl, data: Arc<Mutex<IndexData>>, cancel: Arc<AtomicBool>, repaint: Repaint) {
    let mut buf = vec![0u8; BLOCK as usize];
    let mut last_tail: Option<u64> = None;
    let mut last_repaint = Instant::now();
    while !cancel.load(Ordering::Relaxed) {
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let (full_blocks, base) = {
            let mut d = data.lock().unwrap();
            if size < d.indexed {
                // Файл обрезан (ротация) — начинаем заново.
                *d = IndexData { checkpoints: vec![0], ..Default::default() };
                last_tail = None;
            }
            ((d.checkpoints.len() - 1) as u64, *d.checkpoints.last().unwrap())
        };
        let start = full_blocks * BLOCK;

        if start + BLOCK <= size {
            let Ok(n) = read_at(&file, &mut buf, start) else { break };
            if (n as u64) < BLOCK {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            let count = nl.count(&buf);
            let mut d = data.lock().unwrap();
            d.checkpoints.push(base + count);
            d.tail_newlines = 0;
            d.indexed = start + BLOCK;
            d.ends_with_nl = ends_with(&buf, nl);
            d.complete = false;
            drop(d);
            last_tail = None;
            if last_repaint.elapsed() > Duration::from_millis(100) {
                last_repaint = Instant::now();
                repaint();
            }
            continue;
        }

        // Дошли до хвоста файла.
        let tail = size - start;
        if last_tail != Some(tail) {
            let n = read_at(&file, &mut buf[..tail as usize], start).unwrap_or(0);
            let mut d = data.lock().unwrap();
            d.tail_newlines = nl.count(&buf[..n]);
            d.indexed = start + n as u64;
            if n > 0 {
                d.ends_with_nl = ends_with(&buf[..n], nl);
            }
            d.complete = true;
            drop(d);
            last_tail = Some(tail);
            repaint();
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn ends_with(buf: &[u8], nl: Nl) -> bool {
    match nl {
        Nl::Byte => buf.last() == Some(&b'\n'),
        Nl::U16Le => buf.ends_with(&[b'\n', 0]),
        Nl::U16Be => buf.ends_with(&[0, b'\n']),
    }
}
