//! Parsers for the entrypoints domain.

use super::*;

// --- events.xml ---

pub(crate) struct RawObserver {
    pub name: String,
    pub instance: ClassName,
    pub disabled: Option<bool>,
    pub shared: Option<bool>,
    pub line: u32,
}

/// Parse `events.xml`: `<event name=…><observer name= instance= …/></event>`.
pub(crate) fn events_xml(xml: &str) -> Vec<(EventName, RawObserver)> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut current: Option<EventName> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"event" => current = attr(&e, b"name").map(EventName::new),
                b"observer" => {
                    if let (Some(ev), Some(name), Some(inst)) =
                        (&current, attr(&e, b"name"), attr(&e, b"instance"))
                    {
                        out.push((
                            ev.clone(),
                            RawObserver {
                                name,
                                instance: ClassName::new(inst),
                                disabled: attr(&e, b"disabled").map(|s| matches!(s.trim(), "true" | "1")),
                                shared: attr(&e, b"shared").map(|s| matches!(s.trim(), "true" | "1")),
                                line,
                            },
                        ));
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"event" => current = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

// --- crontab.xml ---

pub(crate) struct RawJob {
    pub group: String,
    pub name: String,
    pub instance: ClassName,
    pub method: String,
    pub schedule: Option<String>,
    pub config_path: Option<String>,
    pub line: u32,
}

/// Parse `crontab.xml`: `<group id=…><job name= instance= method=><schedule>…</schedule></job></group>`.
pub(crate) fn crontab_xml(xml: &str) -> Vec<RawJob> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawJob> = Vec::new();
    let mut group = String::new();
    // Index in `out` of the job currently being filled (for nested <schedule>/<config_path>).
    let mut cur_job: Option<usize> = None;
    let mut text_into: Option<&'static str> = None; // "schedule" | "config_path"
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"group" => group = attr(&e, b"id").unwrap_or_default(),
                b"job" => {
                    out.push(RawJob {
                        group: group.clone(),
                        name: attr(&e, b"name").unwrap_or_default(),
                        instance: ClassName::new(attr(&e, b"instance").unwrap_or_default()),
                        method: attr(&e, b"method").unwrap_or_default(),
                        schedule: None,
                        config_path: None,
                        line,
                    });
                    cur_job = Some(out.len() - 1);
                }
                b"schedule" => text_into = Some("schedule"),
                b"config_path" => text_into = Some("config_path"),
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if let (Some(i), Some(field)) = (cur_job, text_into) {
                    let t = e.unescape().unwrap_or_default().trim().to_string();
                    if !t.is_empty() {
                        match field {
                            "schedule" => out[i].schedule = Some(t),
                            _ => out[i].config_path = Some(t),
                        }
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"job" => cur_job = None,
                b"schedule" | b"config_path" => text_into = None,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    out
}

// --- routes.xml ---

pub(crate) struct RawRoute {
    pub router: String,
    pub id: String,
    pub front_name: String,
    pub modules: Vec<String>,
    pub line: u32,
}

/// Parse `routes.xml`: `<router id=…><route id= frontName=><module name=…/></route></router>`.
pub(crate) fn routes_xml(xml: &str) -> Vec<RawRoute> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawRoute> = Vec::new();
    let mut router = String::new();
    let mut cur: Option<usize> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"router" => router = attr(&e, b"id").unwrap_or_default(),
                b"route" => {
                    out.push(RawRoute {
                        router: router.clone(),
                        id: attr(&e, b"id").unwrap_or_default(),
                        front_name: attr(&e, b"frontName").unwrap_or_default(),
                        modules: Vec::new(),
                        line,
                    });
                    cur = Some(out.len() - 1);
                }
                b"module" => {
                    if let (Some(i), Some(name)) = (cur, attr(&e, b"name")) {
                        out[i].modules.push(name);
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"route" => cur = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

// --- webapi.xml ---

pub(crate) struct RawWebapi {
    pub method: String,
    pub url: String,
    pub service_class: ClassName,
    pub service_method: String,
    pub resources: Vec<String>,
    pub line: u32,
}

/// Parse `webapi.xml`: `<route url= method=><service class= method=/><resources><resource ref=/></resources></route>`.
pub(crate) fn webapi_xml(xml: &str) -> Vec<RawWebapi> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawWebapi> = Vec::new();
    let mut cur: Option<usize> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"route" => {
                    out.push(RawWebapi {
                        method: attr(&e, b"method").unwrap_or_default(),
                        url: attr(&e, b"url").unwrap_or_default(),
                        service_class: ClassName::new(String::new()),
                        service_method: String::new(),
                        resources: Vec::new(),
                        line,
                    });
                    cur = Some(out.len() - 1);
                }
                b"service" => {
                    if let Some(i) = cur {
                        if let Some(c) = attr(&e, b"class") {
                            out[i].service_class = ClassName::new(c);
                        }
                        if let Some(m) = attr(&e, b"method") {
                            out[i].service_method = m;
                        }
                    }
                }
                b"resource" => {
                    if let (Some(i), Some(r)) = (cur, attr(&e, b"ref")) {
                        out[i].resources.push(r);
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"route" => cur = None,
            _ => {}
        }
        buf.clear();
    }
    out
}
