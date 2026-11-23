//! Voice calls: the `+CLCC` call list and the state changes `ATA`, `ATH`, `AT+CHLD` and the
//! host console make to it.

/// Call states as numbered by TS 27.007 `+CLCC` (and the radio HAL's `Call.state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallState {
    Active = 0,
    Held = 1,
    /// Outgoing, not yet ringing at the far end.
    Dialing = 2,
    /// Outgoing, ringing at the far end.
    Alerting = 3,
    /// Incoming, no other call in progress.
    Incoming = 4,
    /// Incoming while another call is in progress.
    Waiting = 5,
}

impl CallState {
    /// The word the stock console's `gsm list` prints.
    pub fn console_name(self) -> &'static str {
        match self {
            CallState::Active => "active",
            CallState::Held => "held",
            CallState::Alerting => "ringing",
            CallState::Waiting => "waiting",
            CallState::Incoming => "incoming",
            CallState::Dialing => "unknown",
        }
    }

    fn ringing(self) -> bool {
        matches!(self, CallState::Incoming | CallState::Waiting)
    }

    /// The foreground: what `AT+CHLD=1` releases.
    fn foreground(self) -> bool {
        matches!(self, CallState::Active | CallState::Dialing | CallState::Alerting)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// `+CLCC` index, 1..=7.
    pub index: u8,
    /// Mobile terminated (incoming).
    pub inbound: bool,
    pub state: CallState,
    pub number: String,
    pub multiparty: bool,
}

pub const MAX_CALLS: usize = 7;

#[derive(Default)]
pub struct Calls {
    list: Vec<Call>,
}

impl Calls {
    pub fn list(&self) -> &[Call] {
        &self.list
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Adds a call with the lowest free index; `None` when all indices are taken.
    pub fn add(&mut self, inbound: bool, number: &str) -> Option<&Call> {
        let index = (1..=MAX_CALLS as u8).find(|i| self.list.iter().all(|c| c.index != *i))?;
        let state = match (inbound, self.list.is_empty()) {
            (true, true) => CallState::Incoming,
            (true, false) => CallState::Waiting,
            (false, _) => CallState::Dialing,
        };
        self.list.push(Call { index, inbound, state, number: number.to_string(), multiparty: false });
        self.list.sort_by_key(|c| c.index);
        self.list.iter().find(|c| c.index == index)
    }

    /// The call to or from `number` (a leading `+` is ignored when comparing).
    pub fn find_mut(&mut self, number: &str) -> Option<&mut Call> {
        let want = number.trim_start_matches('+');
        self.list.iter_mut().find(|c| c.number.trim_start_matches('+') == want)
    }

    pub fn remove_number(&mut self, number: &str) -> bool {
        let want = number.trim_start_matches('+').to_string();
        let before = self.list.len();
        self.list.retain(|c| c.number.trim_start_matches('+') != want);
        self.fixup();
        self.list.len() != before
    }

    /// Moves outgoing calls the guest has seen as dialing to alerting; true if any moved.
    pub fn advance_dialing(&mut self) -> bool {
        let mut moved = false;
        for c in &mut self.list {
            if c.state == CallState::Dialing {
                c.state = CallState::Alerting;
                moved = true;
            }
        }
        moved
    }

    /// `ATA`: answer the ringing call, holding whatever was active.
    pub fn answer(&mut self) {
        if !self.list.iter().any(|c| c.state.ringing()) {
            return;
        }
        self.set_where(|s| s == CallState::Active, CallState::Held);
        if let Some(c) = self.list.iter_mut().find(|c| c.state.ringing()) {
            c.state = CallState::Active;
        }
        self.fixup();
    }

    /// `ATH`: reject the ringing call if there is one, otherwise hang up everything.
    pub fn hangup(&mut self) {
        if self.list.iter().any(|c| c.state.ringing()) {
            self.list.retain(|c| !c.state.ringing());
        } else {
            self.list.clear();
        }
        self.fixup();
    }

    /// `AT+CHLD=<n>[<x>]` (TS 22.030 / 27.007 call hold and multiparty). Returns false for an
    /// unsupported argument.
    pub fn chld(&mut self, arg: &str) -> bool {
        let mut chars = arg.chars();
        let Some(op) = chars.next() else {
            return false;
        };
        let target: Option<u8> = match chars.as_str() {
            "" => None,
            x => match x.parse() {
                Ok(i) => Some(i),
                Err(_) => return false,
            },
        };
        match (op, target) {
            // Release held calls, or reject the waiting one (UDUB).
            ('0', None) => {
                if self.list.iter().any(|c| c.state.ringing()) {
                    self.list.retain(|c| !c.state.ringing());
                } else {
                    self.list.retain(|c| c.state != CallState::Held);
                }
            }
            // Release the foreground and accept the waiting call, else resume the held one.
            ('1', None) => {
                self.list.retain(|c| !c.state.foreground());
                if self.list.iter().any(|c| c.state.ringing()) {
                    if let Some(c) = self.list.iter_mut().find(|c| c.state.ringing()) {
                        c.state = CallState::Active;
                    }
                } else {
                    self.set_where(|s| s == CallState::Held, CallState::Active);
                }
            }
            ('1', Some(x)) => self.list.retain(|c| c.index != x),
            // Hold the active calls and accept the waiting one, else swap active and held.
            ('2', None) => {
                if self.list.iter().any(|c| c.state.ringing()) {
                    self.set_where(|s| s == CallState::Active, CallState::Held);
                    if let Some(c) = self.list.iter_mut().find(|c| c.state.ringing()) {
                        c.state = CallState::Active;
                    }
                } else {
                    for c in &mut self.list {
                        c.state = match c.state {
                            CallState::Active => CallState::Held,
                            CallState::Held => CallState::Active,
                            s => s,
                        };
                    }
                }
            }
            // Private consultation: hold everything active except call x.
            ('2', Some(x)) => {
                for c in &mut self.list {
                    if c.index == x {
                        c.state = CallState::Active;
                        c.multiparty = false;
                    } else if c.state == CallState::Active {
                        c.state = CallState::Held;
                    }
                }
            }
            // Join held calls to the conversation.
            ('3', None) => {
                self.set_where(|s| s == CallState::Held, CallState::Active);
                let active = self.list.iter().filter(|c| c.state == CallState::Active).count();
                if active > 1 {
                    for c in &mut self.list {
                        if c.state == CallState::Active {
                            c.multiparty = true;
                        }
                    }
                }
            }
            // Explicit call transfer: both parties leave.
            ('4', None) => self.list.retain(|c| !matches!(c.state, CallState::Active | CallState::Held)),
            _ => return false,
        }
        self.fixup();
        true
    }

    fn set_where(&mut self, pred: impl Fn(CallState) -> bool, to: CallState) {
        for c in &mut self.list {
            if pred(c.state) {
                c.state = to;
            }
        }
    }

    /// A conference of one is no conference; a lone incoming call is not waiting.
    fn fixup(&mut self) {
        let mpty = self.list.iter().filter(|c| c.multiparty).count();
        if mpty < 2 {
            for c in &mut self.list {
                c.multiparty = false;
            }
        }
        if self.list.len() == 1 && self.list[0].state == CallState::Waiting {
            self.list[0].state = CallState::Incoming;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(c: &Calls) -> Vec<(u8, CallState)> {
        c.list().iter().map(|c| (c.index, c.state)).collect()
    }

    #[test]
    fn waiting_call_swap_and_conference() {
        let mut c = Calls::default();
        c.add(true, "111");
        c.answer();
        c.add(true, "222");
        assert_eq!(states(&c), [(1, CallState::Active), (2, CallState::Waiting)]);
        assert!(c.chld("2"));
        assert_eq!(states(&c), [(1, CallState::Held), (2, CallState::Active)]);
        assert!(c.chld("3"));
        assert!(c.list().iter().all(|x| x.multiparty && x.state == CallState::Active));
        assert!(c.chld("12"));
        assert_eq!(states(&c), [(1, CallState::Active)]);
        assert!(!c.list()[0].multiparty);
        assert!(c.chld("1"));
        assert!(c.is_empty());
        assert!(!c.chld("9"));
    }

    #[test]
    fn hangup_rejects_ringing_first() {
        let mut c = Calls::default();
        c.add(false, "111");
        c.add(true, "222");
        c.hangup();
        assert_eq!(states(&c), [(1, CallState::Dialing)]);
        c.hangup();
        assert!(c.is_empty());
    }
}
