use crate::storage_layer::JsonStorage;
use chrono::Local;
use serde::ser::{SerializeMap, Serializer};
use serde_json::Value;
use std::fmt;
use std::io::Write;
use tracing::{Event, Id, Subscriber};
use tracing_core::span::Attributes;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::SpanRef;
use tracing_subscriber::Layer;

pub struct JsonFormattingLayer<W: for<'a> MakeWriter<'a> + 'static> {
    make_writer: W,
    pid: u32,
    hostname: String,
    name: String,
}

#[derive(Debug)]
pub struct Config {
    pub offset: i8,
}

impl<W: for<'a> MakeWriter<'a> + 'static> JsonFormattingLayer<W> {
    pub fn new(name: String, make_writer: W) -> Self {
        Self::with_default_fields(name, make_writer)
    }

    pub fn with_default_fields(name: String, make_writer: W) -> Self {
        Self {
            make_writer,
            name,
            pid: std::process::id(),
            hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        }
    }

    fn serialize_span<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>(
        &self,
        span: &SpanRef<S>,
        ty: Type,
    ) -> Result<Vec<u8>, std::io::Error> {
        let mut buffer = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut buffer);
        let mut map_serializer = serializer.serialize_map(None)?;
        let message = format_span_context(span, ty);
        map_serializer.serialize_entry(
            "time",
            &Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        )?;
        map_serializer.serialize_entry("name", &self.name)?;
        map_serializer.serialize_entry("host", &self.hostname)?;
        map_serializer.serialize_entry("message", &message)?;
        map_serializer.serialize_entry("level", &span.metadata().level().to_string())?;
        map_serializer.serialize_entry("pid", &self.pid)?;
        map_serializer.serialize_entry("target", span.metadata().target())?;
        map_serializer.serialize_entry("line", &span.metadata().line())?;
        map_serializer.serialize_entry("file", &span.metadata().file())?;

        let extensions = span.extensions();
        if let Some(visitor) = extensions.get::<JsonStorage>() {
            for (key, value) in visitor.values() {
                map_serializer.serialize_entry(key, value)?;
            }
        }
        map_serializer.end()?;
        buffer.write_all(b"\n")?;
        Ok(buffer)
    }

    fn emit(&self, buffer: &[u8]) -> Result<(), std::io::Error> {
        self.make_writer.make_writer().write_all(buffer)
    }
}

#[derive(Clone, Debug)]
pub enum Type {
    EnterSpan,
    ExitSpan,
    Event,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let repr = match self {
            Type::EnterSpan => "START",
            Type::ExitSpan => "END",
            Type::Event => "EVENT",
        };
        write!(f, "{}", repr)
    }
}

fn format_span_context<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>(
    span: &SpanRef<S>,
    ty: Type,
) -> String {
    format!("[{} - {}]", span.metadata().name().to_uppercase(), ty)
}

fn format_event_message<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>(
    current_span: &Option<SpanRef<S>>,
    event: &Event,
    event_visitor: &JsonStorage<'_>,
) -> String {
    let mut message = event_visitor
        .values()
        .get("message")
        .and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| event.metadata().target())
        .to_owned();

    if let Some(span) = &current_span {
        message = format!("{} {}", format_span_context(span, Type::Event), message);
    }

    message
}

impl<S, W> Layer<S> for JsonFormattingLayer<W>
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let current_span = ctx.lookup_current();

        let mut event_visitor = JsonStorage::default();
        event.record(&mut event_visitor);

        let format = || {
            let mut buffer = Vec::new();

            let mut serializer = serde_json::Serializer::new(&mut buffer);
            let mut map_serializer = serializer.serialize_map(None)?;

            let message = format_event_message(&current_span, event, &event_visitor);
            map_serializer.serialize_entry(
                "time",
                &Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            )?;
            map_serializer.serialize_entry("name", &self.name)?;
            map_serializer.serialize_entry("host", &self.hostname)?;
            map_serializer.serialize_entry("message", &message)?;
            map_serializer.serialize_entry("level", &event.metadata().level().to_string())?;
            map_serializer.serialize_entry("pid", &self.pid)?;
            map_serializer.serialize_entry("target", event.metadata().target())?;
            map_serializer.serialize_entry("line", &event.metadata().line())?;
            map_serializer.serialize_entry("file", &event.metadata().file())?;

            for (key, value) in event_visitor
                .values()
                .iter()
                .filter(|(&key, _)| key != "message")
            {
                map_serializer.serialize_entry(key, value)?;
            }

            if let Some(span) = &current_span {
                let extensions = span.extensions();
                if let Some(visitor) = extensions.get::<JsonStorage>() {
                    for (key, value) in visitor.values() {
                        map_serializer.serialize_entry(key, value)?;
                    }
                }
            }
            map_serializer.end()?;
            buffer.write_all(b"\n")?;

            Ok(buffer)
        };

        let result: std::io::Result<Vec<u8>> = format();
        if let Ok(formatted) = result {
            let _ = self.emit(&formatted);
        }
    }

    fn on_new_span(&self, _attrs: &Attributes, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        if let Ok(serialized) = self.serialize_span(&span, Type::EnterSpan) {
            let _ = self.emit(&serialized);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let span = ctx.span(&id).expect("Span not found, this is a bug");
        if let Ok(serialized) = self.serialize_span(&span, Type::ExitSpan) {
            let _ = self.emit(&serialized);
        }
    }
}
