use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::panic::{catch_unwind, UnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
/// Tiny thread-backed background job thing
///
/// This is very similar to ffi_helper's Task
/// https://github.com/Michael-F-Bryan/ffi_helpers
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ControlToken<S>(Arc<ControlTokenInner<S>>);

#[derive(Debug, Default)]
pub struct ControlTokenInner<S> {
    cancel: AtomicBool,
    pause: AtomicBool,
    status: Mutex<Option<S>>,
}

impl<S> ControlToken<S> {
    pub fn new() -> Self {
        Self(Arc::new(ControlTokenInner {
            cancel: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            status: Mutex::new(None),
        }))
    }

    pub fn cancel(&self) {
        self.0.cancel.store(true, Ordering::SeqCst);
    }

    pub fn pause(&self) {
        self.0.pause.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.0.pause.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancel.load(Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.0.pause.load(Ordering::SeqCst)
    }

    pub fn is_cancelled_with_pause(&self) -> bool {
        self.is_cancelled() || (self.handle_pause() && self.is_cancelled())
    }

    pub fn handle_pause(&self) -> bool {
        let mut paused = false;

        while self.is_paused() && !self.is_cancelled() {
            paused = true;
            thread::park();
        }

        paused
    }

    pub fn set_status(&self, status: S) {
        let mut previous = self.0.status.lock().expect("status lock");
        previous.replace(status);
    }

    pub fn get_status(&self) -> Option<S> {
        let mut current = self.0.status.lock().expect("status lock");
        current.take()
    }

    #[cfg(test)]
    pub fn result(&self) -> Result<(), ()> {
        if self.is_cancelled() {
            Err(())
        } else {
            Ok(())
        }
    }
}

impl<S> Default for ControlToken<S> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct BackgroundHandle<T, S> {
    result: Receiver<std::thread::Result<T>>,
    control: ControlToken<S>,
    thread: thread::Thread,
}

impl<T, S> BackgroundHandle<T, S> {
    pub fn spawn<K>(task: K) -> BackgroundHandle<T, S>
    where
        K: Background<Output = T, Status = S> + UnwindSafe + Send + Sync + 'static,
        T: Send + Sync + 'static,
        S: Send + Sync + Clone + 'static,
    {
        let (tx, rx) = crossbeam_channel::unbounded();
        let control = ControlToken::new();
        let inner_control = control.clone();

        let handle = thread::spawn(move || {
            let response = catch_unwind(move || task.run(&inner_control));
            let _ = tx.send(response);
        });

        let thread = handle.thread().clone();

        BackgroundHandle {
            result: rx,
            control,
            thread,
        }
    }

    pub fn wait_timeout(&self, wait: Duration) -> Option<T> {
        match self.result.recv_timeout(wait) {
            Ok(value) => Some(value.unwrap()),
            Err(RecvTimeoutError::Timeout) => None,
            Err(e) => panic!("{:?}", e),
        }
    }

    /// Inspect completion without consuming it, so coordinators can detect a
    /// worker that exited before reporting its active file.
    pub fn is_finished(&self) -> bool {
        !self.result.is_empty()
    }

    pub fn wait_result(self) -> std::thread::Result<T> {
        self.result
            .recv()
            .unwrap_or_else(|_| Err(Box::new("Background worker disconnected before returning")))
    }

    pub fn cancel(&self) {
        self.control.cancel();
        self.thread.unpark();
    }

    pub fn status(&self) -> Option<S> {
        self.control.get_status()
    }

    pub fn pause(&self) {
        self.control.pause();
    }

    pub fn resume(&self) {
        self.control.resume();
        self.thread.unpark();
    }
}

impl<T, S> Drop for BackgroundHandle<T, S> {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub trait Background: Send + Sync {
    type Output: Send + Sync;
    type Status: Send + Sync;

    fn run(self, control: &ControlToken<Self::Status>) -> Self::Output;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Stepped {
        steps: Receiver<()>,
        events: crossbeam_channel::Sender<u32>,
    }

    impl Background for Stepped {
        type Output = Result<u32, u32>;
        type Status = u32;

        fn run(self, control: &ControlToken<Self::Status>) -> Self::Output {
            let mut count = 0;
            while !control.is_cancelled_with_pause() {
                match self.steps.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) => {
                        if control.is_cancelled_with_pause() {
                            break;
                        }
                        count += 1;
                        control.set_status(count);
                        self.events.send(count).unwrap();
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            control.result().map(|_| count).map_err(|_| count)
        }
    }

    #[test]
    fn cancellation_finishes_after_the_acknowledged_step() {
        let (send, steps) = crossbeam_channel::unbounded();
        let (events, receive) = crossbeam_channel::unbounded();
        let handle = BackgroundHandle::spawn(Stepped { steps, events });
        send.send(()).unwrap();
        assert_eq!(1, receive.recv_timeout(Duration::from_secs(2)).unwrap());
        handle.cancel();
        assert_eq!(Some(Err(1)), handle.wait_timeout(Duration::from_secs(2)));
    }

    #[test]
    fn pause_stops_work_and_cancel_wakes_a_paused_task() {
        let (send, steps) = crossbeam_channel::unbounded();
        let (events, receive) = crossbeam_channel::unbounded();
        let handle = BackgroundHandle::spawn(Stepped { steps, events });
        handle.pause();
        send.send(()).unwrap();
        assert!(matches!(
            receive.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        ));
        handle.resume();
        assert_eq!(1, receive.recv_timeout(Duration::from_secs(2)).unwrap());
        handle.pause();
        send.send(()).unwrap();
        assert!(matches!(
            receive.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        ));
        handle.cancel();
        assert_eq!(Some(Err(1)), handle.wait_timeout(Duration::from_secs(2)));
    }
}
