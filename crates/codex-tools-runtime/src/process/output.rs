use crate::upstream_head_tail_buffer::HeadTailBuffer;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};

pub(crate) struct OutputState {
    buffer: Mutex<HeadTailBuffer>,
    notify: Notify,
    closed_notify: Notify,
    closed: AtomicBool,
    readers: AtomicUsize,
    exit: Mutex<ExitState>,
}

#[derive(Clone, Copy)]
enum ExitState {
    Running,
    Exited(Option<i32>),
}

impl OutputState {
    pub(crate) fn new(readers: usize) -> Self {
        Self {
            buffer: Mutex::new(HeadTailBuffer::default()),
            notify: Notify::new(),
            closed_notify: Notify::new(),
            closed: AtomicBool::new(false),
            readers: AtomicUsize::new(readers),
            exit: Mutex::new(ExitState::Running),
        }
    }

    pub(crate) fn push(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.buffer.lock().unwrap().push_chunk(bytes);
        self.notify.notify_waiters();
    }

    pub(crate) fn reader_closed(&self) {
        self.readers.fetch_sub(1, Ordering::AcqRel);
        self.finish_if_ready();
    }

    pub(crate) fn mark_exit(&self, exit_code: Option<i32>) {
        *self.exit.lock().unwrap() = ExitState::Exited(exit_code);
        self.finish_if_ready();
    }

    fn finish_if_ready(&self) {
        if self.readers.load(Ordering::Acquire) == 0
            && matches!(*self.exit.lock().unwrap(), ExitState::Exited(_))
            && !self.closed.swap(true, Ordering::AcqRel)
        {
            self.notify.notify_waiters();
            self.closed_notify.notify_waiters();
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn exit_code(&self) -> Option<i32> {
        match *self.exit.lock().unwrap() {
            ExitState::Running | ExitState::Exited(None) => None,
            ExitState::Exited(exit_code) => exit_code,
        }
    }

    pub(crate) async fn wait_closed(&self) {
        loop {
            let notified = self.closed_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_closed() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn drain_now(&self) -> HeadTailBuffer {
        self.buffer.lock().unwrap().drain()
    }

    pub(crate) fn restore(&self, staged: HeadTailBuffer) {
        let mut buffer = self.buffer.lock().unwrap();
        let newer = buffer.drain();
        *buffer = staged;
        buffer.push_buffer(newer);
        self.notify.notify_waiters();
    }

    pub(crate) async fn collect_until(&self, deadline: Instant) -> HeadTailBuffer {
        let mut collected = HeadTailBuffer::default();
        loop {
            let output_notified = self.notify.notified();
            let closed_notified = self.closed_notify.notified();
            tokio::pin!(output_notified);
            tokio::pin!(closed_notified);
            output_notified.as_mut().enable();
            closed_notified.as_mut().enable();

            collected.push_buffer(self.drain_now());
            if self.is_closed() {
                collected.push_buffer(self.drain_now());
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining == Duration::ZERO {
                break;
            }
            tokio::select! {
                () = &mut output_notified => {}
                () = &mut closed_notified => {}
                () = tokio::time::sleep(remaining) => break,
            }
        }
        collected
    }
}

pub(crate) fn render_staged(
    staged: &HeadTailBuffer,
    max_output_tokens: Option<usize>,
) -> (String, usize) {
    let original_token_count = staged.total_bytes().saturating_add(3) / 4;
    let raw = String::from_utf8_lossy(&staged.to_bytes_with_omission_marker()).into_owned();
    let max_tokens = max_output_tokens.unwrap_or(super::DEFAULT_MAX_OUTPUT_TOKENS);
    let byte_budget = max_tokens.saturating_mul(4);
    if raw.len() <= byte_budget {
        return (raw, original_token_count);
    }

    let (head_budget, tail_budget) = (byte_budget / 2, byte_budget.saturating_sub(byte_budget / 2));
    let head_end = previous_char_boundary(&raw, head_budget);
    let tail_start = next_char_boundary(&raw, raw.len().saturating_sub(tail_budget));
    let omitted = raw[head_end..tail_start].len().saturating_add(3) / 4;
    let truncated = format!(
        "{}…{omitted} tokens truncated…{}",
        &raw[..head_end],
        &raw[tail_start..]
    );
    let collection_notice = if staged.omitted_bytes() > 0 {
        let marker = crate::unified_exec::format_output_omission_marker(staged.omitted_bytes());
        if truncated.contains(&marker) {
            String::new()
        } else {
            format!("{marker}\n")
        }
    } else {
        String::new()
    };
    (
        format!(
            "Warning: truncated output (original token count: {original_token_count})\n{collection_notice}\n{truncated}"
        ),
        original_token_count,
    )
}

fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::OutputState;
    use std::sync::Arc;
    use tokio::time::{Duration, Instant};

    #[tokio::test]
    async fn waits_for_terminal_transition_after_registering() {
        let output = Arc::new(OutputState::new(0));
        let waiter = {
            let output = Arc::clone(&output);
            tokio::spawn(async move { output.wait_closed().await })
        };

        tokio::task::yield_now().await;
        output.mark_exit(Some(0));
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("waiter should observe closure")
            .expect("waiter task should not panic");
    }

    #[tokio::test]
    async fn collection_observes_output_after_registering() {
        let output = Arc::new(OutputState::new(1));
        let collector = {
            let output = Arc::clone(&output);
            tokio::spawn(async move {
                output
                    .collect_until(Instant::now() + Duration::from_secs(1))
                    .await
            })
        };

        tokio::task::yield_now().await;
        output.push(b"ready".to_vec());
        output.reader_closed();
        output.mark_exit(Some(0));
        let collected = tokio::time::timeout(Duration::from_millis(100), collector)
            .await
            .expect("collector should observe output")
            .expect("collector task should not panic");
        assert_eq!(collected.to_bytes(), b"ready");
    }
}
