//! Machine groups: machines that admit the devices any member admits, so a
//! device paired with one member reaches all of them.
//!
//! Every member is equal; there is no coordinator and no group key. Each
//! machine keeps the shared [`GroupState`] in `traverse.json` beside its own
//! allow list, which stays the only admission check. Members exchange the
//! whole state over `tcode/group/1` and merge it entry by entry: the higher
//! `(counter, removed, by)` wins, so at an equal counter a removal beats an
//! admission, and a removed device returns only through a new pairing, which
//! writes the next counter.
//!
//! Inside a group there is one way in: a member's one-time invitation, which
//! then names the group. A device pairing with it joins the group, and so
//! does a machine presenting it over `tcode/group/1`. Every full-access
//! device of a member is a member; only space members stay on their machine.
use serde::{Deserialize, Serialize};
use tcode_client::pairing::{MAX_ADDRS, MAX_TRAVERSE, valid_host_id, valid_invitation_secret};

pub const ALPN_GROUP: &[u8] = b"tcode/group/1";
pub const MAX_GROUP_MACHINES: usize = 64;
pub const MAX_GROUP_DEVICES: usize = 1024;
/// Bound on one `tcode/group/1` line: the whole state at its largest.
pub const MAX_GROUP_LINE: usize = 512 * 1024;
/// Bound on a pairing or hello reply that carries the group's machines.
pub const MAX_ROSTER_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    Member,
    Removed,
}

/// The last change to one machine's or device's place in the group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub state: Membership,
    pub counter: u64,
    /// The machine that made the change.
    pub by: String,
    pub changed_unix: u64,
}

impl Entry {
    fn rank(&self) -> (u64, bool, &str) {
        (self.counter, self.state == Membership::Removed, &self.by)
    }

    pub fn is_member(&self) -> bool {
        self.state == Membership::Member
    }
}

/// A member machine and where to dial it. The routing hints are the
/// machine's own report, refreshed by the dialer from the connections it
/// makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMachine {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traverse: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    pub entry: Entry,
}

impl GroupMachine {
    /// A machine with these hints; its entry is written by
    /// [`GroupState::set_machine`].
    pub fn new(
        id: String,
        name: String,
        traverse: Vec<String>,
        relay: Option<String>,
        addrs: Vec<String>,
    ) -> Self {
        Self {
            id,
            name,
            traverse,
            relay,
            addrs,
            entry: Entry {
                state: Membership::Member,
                counter: 0,
                by: String::new(),
                changed_unix: 0,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDevice {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub entry: Entry,
}

/// What members exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupState {
    pub id: String,
    pub name: String,
    pub machines: Vec<GroupMachine>,
    pub devices: Vec<GroupDevice>,
}

/// A device admission this machine declined while **Accept new devices**
/// was off. It stays declined until the user admits it here or a newer
/// pairing elsewhere writes a later counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declined {
    pub id: String,
    pub counter: u64,
}

/// The group as `traverse.json` holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRecord {
    #[serde(flatten)]
    pub state: GroupState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declined: Vec<Declined>,
}

/// One machine of the group as devices store it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterMachine {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traverse: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    pub member: bool,
}

/// The group's machines, sent to a full device with `paired` and `hello_ok`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    pub id: String,
    pub name: String,
    pub machines: Vec<RosterMachine>,
}

/// How a joining machine describes itself and the devices it brings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoiningMachine {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traverse: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<JoiningDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoiningDevice {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// The lines of `tcode/group/1`: the dialer sends one, the accepter answers
/// with one, and the connection ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GroupLine {
    /// Present the secret of a member's invitation to join as a machine.
    Join {
        group: String,
        secret: String,
        machine: JoiningMachine,
    },
    /// The dialer's state, answered with the accepter's state after merging.
    Sync {
        state: GroupState,
    },
    Joined {
        state: GroupState,
    },
    Rejected {
        reason: GroupRejection,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRejection {
    /// Wrong, expired or already used invitation.
    Invalid,
    /// The machine is not accepting new devices.
    Disabled,
    /// The machine could not record the change; try again.
    Busy,
    /// The dialer is not a member, or the group is not this machine's.
    NotMember,
    Full,
}

impl JoiningMachine {
    pub fn is_valid(&self) -> bool {
        valid_name(&self.name)
            && valid_hints(&self.traverse, self.relay.as_deref(), &self.addrs)
            && self.devices.len() <= MAX_GROUP_DEVICES
            && self.devices.iter().all(|device| {
                valid_host_id(&device.id)
                    && valid_name(&device.name)
                    && device
                        .platform
                        .as_deref()
                        .is_none_or(|platform| platform.len() <= 256)
            })
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
}

fn valid_hints(traverse: &[String], relay: Option<&str>, addrs: &[String]) -> bool {
    traverse.len() <= MAX_TRAVERSE
        && traverse.iter().all(|value| value.len() <= 512)
        && relay.is_none_or(|relay| relay.len() <= 512)
        && addrs.len() <= MAX_ADDRS
        && addrs
            .iter()
            .all(|addr| addr.parse::<std::net::SocketAddr>().is_ok())
}

fn valid_entry(entry: &Entry) -> bool {
    valid_host_id(&entry.by)
}

impl GroupState {
    pub fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            machines: Vec::new(),
            devices: Vec::new(),
        }
    }

    /// Whether a state received from another machine is well formed.
    pub fn is_valid(&self) -> bool {
        valid_invitation_secret(&self.id)
            && valid_name(&self.name)
            && self.machines.len() <= MAX_GROUP_MACHINES
            && self.devices.len() <= MAX_GROUP_DEVICES
            && self.machines.iter().all(|machine| {
                valid_host_id(&machine.id)
                    && valid_name(&machine.name)
                    && valid_hints(&machine.traverse, machine.relay.as_deref(), &machine.addrs)
                    && valid_entry(&machine.entry)
            })
            && self.devices.iter().all(|device| {
                valid_host_id(&device.id)
                    && valid_name(&device.name)
                    && device
                        .platform
                        .as_deref()
                        .is_none_or(|platform| platform.len() <= 256)
                    && valid_entry(&device.entry)
            })
    }

    pub fn machine(&self, id: &str) -> Option<&GroupMachine> {
        self.machines.iter().find(|machine| machine.id == id)
    }

    pub fn device(&self, id: &str) -> Option<&GroupDevice> {
        self.devices.iter().find(|device| device.id == id)
    }

    pub fn is_member_machine(&self, id: &str) -> bool {
        self.machine(id)
            .is_some_and(|machine| machine.entry.is_member())
    }

    pub fn member_machines(&self) -> impl Iterator<Item = &GroupMachine> {
        self.machines
            .iter()
            .filter(|machine| machine.entry.is_member())
    }

    fn next_entry(previous: Option<&Entry>, state: Membership, by: &str) -> Entry {
        Entry {
            state,
            counter: previous.map_or(1, |entry| entry.counter + 1),
            by: by.to_owned(),
            changed_unix: crate::identity::now_unix(),
        }
    }

    /// Record a change to a device's place in the group, made by `by`.
    pub fn set_device(
        &mut self,
        id: &str,
        name: &str,
        platform: Option<&str>,
        state: Membership,
        by: &str,
    ) {
        match self.devices.iter_mut().find(|device| device.id == id) {
            Some(device) => {
                device.entry = Self::next_entry(Some(&device.entry), state, by);
                device.name = name.to_owned();
                device.platform = platform.map(str::to_owned);
            }
            None => self.devices.push(GroupDevice {
                id: id.to_owned(),
                name: name.to_owned(),
                platform: platform.map(str::to_owned),
                entry: Self::next_entry(None, state, by),
            }),
        }
    }

    /// Record a change to a machine's place in the group, made by `by`,
    /// with the routing hints it has now.
    pub fn set_machine(&mut self, machine: GroupMachine, state: Membership, by: &str) {
        match self
            .machines
            .iter_mut()
            .find(|known| known.id == machine.id)
        {
            Some(known) => {
                let entry = Self::next_entry(Some(&known.entry), state, by);
                *known = GroupMachine { entry, ..machine };
            }
            None => self.machines.push(GroupMachine {
                entry: Self::next_entry(None, state, by),
                ..machine
            }),
        }
    }

    /// Take every entry of `other` that outranks this state's. Returns
    /// whether anything changed. Entries past the size limits are dropped.
    pub fn merge(&mut self, other: GroupState) -> bool {
        let mut changed = false;
        for machine in other.machines {
            match self
                .machines
                .iter()
                .position(|known| known.id == machine.id)
            {
                Some(index) => {
                    if machine.entry.rank() > self.machines[index].entry.rank() {
                        self.machines[index] = machine;
                        changed = true;
                    }
                }
                None if self.machines.len() < MAX_GROUP_MACHINES => {
                    self.machines.push(machine);
                    changed = true;
                }
                None => {}
            }
        }
        for device in other.devices {
            match self.devices.iter().position(|known| known.id == device.id) {
                Some(index) => {
                    if device.entry.rank() > self.devices[index].entry.rank() {
                        self.devices[index] = device;
                        changed = true;
                    }
                }
                None if self.devices.len() < MAX_GROUP_DEVICES => {
                    self.devices.push(device);
                    changed = true;
                }
                None => {}
            }
        }
        changed
    }

    /// Put where a connection just reached `id` ahead of its hints. These
    /// are this machine's own knowledge and change no entry. Returns whether
    /// anything changed.
    pub fn learn_hints(&mut self, id: &str, relay: Option<String>, addrs: Vec<String>) -> bool {
        let Some(machine) = self.machines.iter_mut().find(|machine| machine.id == id) else {
            return false;
        };
        let before = (machine.relay.clone(), machine.addrs.clone());
        if relay.is_some() {
            machine.relay = relay;
        }
        for addr in addrs.into_iter().rev() {
            machine.addrs.retain(|known| *known != addr);
            machine.addrs.insert(0, addr);
        }
        machine.addrs.truncate(MAX_ADDRS);
        (machine.relay.clone(), machine.addrs.clone()) != before
    }

    /// The machines a device stores, this one included.
    pub fn roster(&self) -> Roster {
        Roster {
            id: self.id.clone(),
            name: self.name.clone(),
            machines: self
                .machines
                .iter()
                .map(|machine| RosterMachine {
                    id: machine.id.clone(),
                    name: machine.name.clone(),
                    traverse: machine.traverse.clone(),
                    relay: machine.relay.clone(),
                    addrs: machine.addrs.clone(),
                    member: machine.entry.is_member(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const PHONE: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn state() -> GroupState {
        GroupState::new("AAAAAAAAAAAAAAAAAAAAAA".into(), "Home".into())
    }

    /// Two machines change the same device without hearing of each other:
    /// one admits it again, the other removes it. Whichever order the states
    /// meet in, both end with the removal, and a later pairing still wins.
    #[test]
    fn concurrent_changes_converge_on_the_removal_and_a_later_pairing_wins() {
        let mut origin = state();
        origin.set_device(PHONE, "Phone", None, Membership::Member, A);
        let mut on_a = origin.clone();
        let mut on_b = origin.clone();
        on_a.set_device(PHONE, "Phone", None, Membership::Member, A);
        on_b.set_device(PHONE, "Phone", None, Membership::Removed, B);

        let mut a_then_b = on_a.clone();
        assert!(a_then_b.merge(on_b.clone()));
        let mut b_then_a = on_b.clone();
        assert!(
            !b_then_a.merge(on_a.clone()),
            "the removal already outranks"
        );
        assert_eq!(a_then_b, b_then_a);
        assert!(!a_then_b.device(PHONE).unwrap().entry.is_member());

        a_then_b.set_device(PHONE, "Phone", None, Membership::Member, A);
        assert!(b_then_a.merge(a_then_b.clone()));
        assert!(b_then_a.device(PHONE).unwrap().entry.is_member());
    }

    #[test]
    fn a_state_from_another_machine_must_be_well_formed() {
        let mut valid = state();
        valid.set_device(PHONE, "Phone", None, Membership::Member, A);
        assert!(valid.is_valid());
        let mut bad_device = valid.clone();
        bad_device.devices[0].id = "phone".into();
        assert!(!bad_device.is_valid());
        let mut bad_author = valid.clone();
        bad_author.devices[0].entry.by = String::new();
        assert!(!bad_author.is_valid());
        let mut bad_id = valid;
        bad_id.id = "home".into();
        assert!(!bad_id.is_valid());
    }
}
