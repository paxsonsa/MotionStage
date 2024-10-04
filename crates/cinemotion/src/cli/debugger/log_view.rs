use anyhow::Result;
use chrono::{DateTime, Local};
use ratatui::{
    buffer::Buffer,
    style::Style,
    widgets::{Block, Borders, Widget},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tracing::{
    field::{Field, Visit},
    Subscriber,
};
use tracing_subscriber::{prelude::*, registry::LookupSpan, EnvFilter};

pub fn init_logging() -> Result<Arc<Mutex<RingBuffer<LogEvent>>>, anyhow::Error> {
    const LOG_CAPACITY: usize = 10000;
    let log_buffer = Arc::new(Mutex::new(RingBuffer::new(LOG_CAPACITY)));
    let collector_layer = LogCollector {
        buffer: Arc::clone(&log_buffer),
    };
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::new("info"))
        .with(collector_layer);
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(log_buffer)
}

pub struct LogWidget {
    pub(crate) buffer: Arc<Mutex<RingBuffer<LogEvent>>>,
}

impl Widget for LogWidget {
    fn render(self, area: ratatui::layout::Rect, buf: &mut Buffer) {
        let border = Block::default().borders(Borders::ALL).title("Logs");
        let bordered_area = border.inner(area);
        border.render(area, buf);

        let buffer = self.buffer.lock().unwrap();
        let lines_to_render = bordered_area.height as usize;
        let total_logs = buffer.size;
        let mut logs_iter = buffer.iter();

        if total_logs > lines_to_render {
            logs_iter.nth(total_logs - lines_to_render);
        }

        let max_width = bordered_area.width as usize;
        let mut current_line = 0;

        for event_data in logs_iter.take(lines_to_render) {
            let fields_str = event_data
                .fields
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(", ");

            let line = format!(
                "[{}] [{}] {} {}",
                event_data.timestamp.format("%Y-%m-%d %H:%M:%S"),
                event_data.level,
                event_data.message.as_deref().unwrap_or(""),
                if fields_str.is_empty() {
                    "".to_string()
                } else {
                    format!("{{{}}}", fields_str)
                }
            );

            for chunk in line.chars().collect::<Vec<_>>().chunks(max_width) {
                if current_line >= lines_to_render {
                    break;
                }
                let chunk_str: String = chunk.iter().collect();
                buf.set_string(
                    bordered_area.left(),
                    bordered_area.top() + current_line as u16,
                    chunk_str,
                    Style::default(),
                );
                current_line += 1;
            }
        }
    }
}

pub struct RingBuffer<T> {
    buffer: Vec<Option<T>>,
    capacity: usize,
    start: usize,
    size: usize,
}

impl<T> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        let mut buffer: Vec<Option<T>> = Vec::with_capacity(1000);
        for _ in 0..capacity {
            buffer.push(None);
        }
        Self {
            buffer,
            capacity,
            start: 0,
            size: 0,
        }
    }

    fn push(&mut self, item: T) {
        if self.size < self.capacity {
            let idx = (self.start + self.size) % self.capacity;
            self.buffer[idx] = Some(item);
            self.size += 1;
        } else {
            self.buffer[self.start] = Some(item);
            self.start = (self.start + 1) % self.capacity;
        }
    }
    fn iter(&self) -> RingBufferIter<T> {
        RingBufferIter {
            buffer: &self.buffer,
            capacity: self.capacity,
            start: self.start,
            size: self.size,
            index: 0,
        }
    }
}

struct RingBufferIter<'a, T> {
    buffer: &'a [Option<T>],
    capacity: usize,
    start: usize,
    size: usize,
    index: usize,
}

impl<'a, T> Iterator for RingBufferIter<'a, T>
where
    T: 'a,
{
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.size {
            return None;
        }
        let pos = (self.start + self.index) % self.capacity;
        self.index += 1;
        self.buffer[pos].as_ref()
    }
}

impl<'a, T> DoubleEndedIterator for RingBufferIter<'a, T>
where
    T: 'a,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.index >= self.size {
            return None;
        }
        let pos = (self.start + self.size - 1 - self.index) % self.capacity;
        self.index += 1;
        self.buffer[pos].as_ref()
    }
}

#[derive(Default)]
struct EventVisitor {
    fields: HashMap<String, String>,
    message: Option<String>,
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name().is_empty() {
            self.message = Some(format!("{:?}", value));
        } else {
            self.fields
                .insert(field.name().to_string(), format!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name().is_empty() {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}
pub struct LogEvent {
    timestamp: DateTime<Local>,
    level: tracing::Level,
    message: Option<String>,
    fields: HashMap<String, String>,
}

pub struct LogCollector {
    pub buffer: Arc<Mutex<RingBuffer<LogEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for LogCollector
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let mut buffer = self.buffer.lock().unwrap();
        buffer.push(LogEvent {
            timestamp: Local::now(),
            level: *event.metadata().level(),
            message: visitor.message,
            fields: visitor.fields,
        });
    }
}
