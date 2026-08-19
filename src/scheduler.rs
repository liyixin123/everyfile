use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

type Job = Box<dyn FnOnce() + Send + 'static>;

enum Message {
    Run(Job),
    Stop,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ScheduleError {
    Full,
    Stopped,
}

pub struct BackgroundScheduler {
    sender: Sender<Message>,
    workers: Vec<JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
}

impl BackgroundScheduler {
    pub fn new(worker_count: usize, queue_capacity: usize) -> Self {
        assert!(worker_count > 0, "worker_count must be positive");
        let (sender, receiver) = bounded(queue_capacity);
        let stopped = Arc::new(AtomicBool::new(false));
        let workers = (0..worker_count)
            .map(|index| spawn_worker(index, receiver.clone(), Arc::clone(&stopped)))
            .collect();
        Self {
            sender,
            workers,
            stopped,
        }
    }

    pub fn try_schedule<F>(&self, job: F) -> Result<(), ScheduleError>
    where
        F: FnOnce() + Send + 'static,
    {
        if self.stopped.load(Ordering::Acquire) {
            return Err(ScheduleError::Stopped);
        }
        self.sender
            .try_send(Message::Run(Box::new(job)))
            .map_err(|error| match error {
                TrySendError::Full(_) => ScheduleError::Full,
                TrySendError::Disconnected(_) => ScheduleError::Stopped,
            })
    }
}

impl Drop for BackgroundScheduler {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        for _ in &self.workers {
            let _ = self.sender.send(Message::Stop);
        }
        while let Some(worker) = self.workers.pop() {
            let _ = worker.join();
        }
    }
}

fn spawn_worker(
    index: usize,
    receiver: Receiver<Message>,
    stopped: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("everyfile-worker-{index}"))
        .spawn(move || {
            while let Ok(message) = receiver.recv() {
                match message {
                    Message::Run(job) => job(),
                    Message::Stop => break,
                }
            }
            stopped.store(true, Ordering::Release);
        })
        .expect("background worker must start")
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn blocking_work_runs_off_the_calling_thread() {
        let caller = thread::current().id();
        let scheduler = BackgroundScheduler::new(1, 1);
        let (tx, rx) = mpsc::channel();
        scheduler
            .try_schedule(move || tx.send(thread::current().id()).unwrap())
            .unwrap();
        assert_ne!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), caller);
    }

    #[test]
    fn shutdown_rejects_no_completed_work() {
        let scheduler = BackgroundScheduler::new(1, 1);
        let (tx, rx) = mpsc::channel();
        scheduler
            .try_schedule(move || tx.send(()).unwrap())
            .unwrap();
        drop(scheduler);
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());
    }
}
