use super::{Result, Value, json};

pub(super) struct Turn {
    pub epoch: String,
    pub request: String,
    pub turn: Option<String>,
    pub seq: u64,
    pub cancelled: bool,
    answer: Option<String>,
    terminal: bool,
    bytes: usize,
}
impl Turn {
    pub fn new(epoch: String) -> Self {
        Self {
            epoch,
            request: uuid::Uuid::new_v4().to_string(),
            turn: None,
            seq: 0,
            cancelled: false,
            answer: None,
            terminal: false,
            bytes: 0,
        }
    }
    pub fn control(&self, op: &str, extra: Value) -> Value {
        let mut frame = json!({"type":"turn_control","version":1,"epoch":self.epoch,"request_id":self.request,"op":op});
        if let (Some(base), Some(extra)) = (frame.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        frame
    }
    pub fn bind(&mut self, frame: &Value) -> Result<()> {
        if frame["version"] != 1
            || frame["epoch"] != self.epoch
            || frame["request_id"] != self.request
        {
            return Err("GATEWAY_BINDING_MISMATCH");
        }
        let turn = frame["turn_id"]
            .as_str()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok())
            .ok_or("GATEWAY_BINDING_MISMATCH")?;
        if self.turn.as_deref().is_some_and(|old| old != turn) {
            return Err("GATEWAY_BINDING_MISMATCH");
        }
        self.turn = Some(turn.into());
        Ok(())
    }
    pub fn event(&mut self, frame: &Value) -> Result<Option<(&'static str, Option<String>)>> {
        self.bind(frame)?;
        if frame["type"] != "turn_event" {
            return Err("GATEWAY_PROTOCOL");
        }
        let seq = frame["seq"]
            .as_u64()
            .filter(|s| *s > 0)
            .ok_or("GATEWAY_SEQUENCE")?;
        if seq <= self.seq {
            return Ok(None);
        }
        if self.terminal || seq != self.seq + 1 {
            return Err("GATEWAY_SEQUENCE");
        }
        self.seq = seq;
        self.bytes += frame.to_string().len();
        if self.bytes > 256 * 1024 {
            return Err("GATEWAY_OUTPUT_TOO_LARGE");
        }
        let event = &frame["event"];
        match event["type"].as_str() {
            Some("chunk") => {
                if !event["content"].is_string() {
                    return Err("GATEWAY_PROTOCOL");
                }
            }
            Some("done") => {
                let answer = event["full_response"]
                    .as_str()
                    .filter(|s| s.len() <= 64 * 1024)
                    .ok_or("GATEWAY_PROTOCOL")?;
                if self.answer.is_some() {
                    return Err("GATEWAY_DUPLICATE_DONE");
                }
                self.answer = Some(answer.into());
            }
            Some("error" | "turn_gap") => return Err("GATEWAY_TURN_INCOMPLETE"),
            _ => {}
        }
        if frame["terminal"] == true {
            if event["type"] != "turn_terminal" || frame["stream_complete"] != true {
                return Err("GATEWAY_TURN_INCOMPLETE");
            }
            self.terminal = true;
            return match event["outcome"].as_str() {
                Some("done") => Ok(Some((
                    "end_turn",
                    Some(self.answer.take().ok_or("GATEWAY_MISSING_DONE")?),
                ))),
                Some("aborted") => Ok(Some(("cancelled", None))),
                _ => Err("GATEWAY_TURN_INCOMPLETE"),
            };
        }
        if event["type"] == "turn_terminal" {
            return Err("GATEWAY_TURN_INCOMPLETE");
        }
        Ok(None)
    }
}
