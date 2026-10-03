//! Who may change a group, and what each change leaves it as.
//!
//! Pure, so the committer that builds a change and every member that checks it
//! reach the same answer from the same state.

use common::proto::mls_wire::GroupChange;
use common::proto::mls_wire::GroupRules;
use common::proto::mls_wire::SignedChange;
use serde::Deserialize;
use serde::Serialize;

pub const ROLE_MEMBER: u8 = 0;
pub const ROLE_ADMIN: u8 = 1;
/// Everything an admin may do, plus choosing owners and whether admins appoint admins. A group
/// always has one.
pub const ROLE_OWNER: u8 = 2;

/// Who runs a group and by what rules, signed into its MLS context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupState {
    pub owners:    Vec<[u8; 32]>,
    /// Admins who aren't owners.
    pub admins:    Vec<[u8; 32]>,
    /// The device that normally commits. Takeovers and consented recovery can
    /// race it; the branch journal resolves those competing commits.
    pub committer: [u8; 32],
    pub rules:     GroupRules,
    /// What the commit that made this state did, and who asked for it.
    pub last:      Option<SignedChange>,
}

type Result<T> = std::result::Result<T, &'static str>;

impl GroupState {
    pub fn founded(by: [u8; 32]) -> Self {
        Self {
            owners:    vec![by],
            admins:    Vec::new(),
            committer: by,
            rules:     GroupRules::default(),
            last:      None,
        }
    }

    pub fn role(&self, who: &[u8; 32]) -> u8 {
        if self.owners.contains(who) {
            ROLE_OWNER
        } else if self.admins.contains(who) {
            ROLE_ADMIN
        } else {
            ROLE_MEMBER
        }
    }

    pub fn may_add(&self, who: &[u8; 32]) -> bool {
        self.rules.members_add || self.role(who) >= ROLE_ADMIN
    }

    pub fn may_edit(&self, who: &[u8; 32]) -> bool {
        self.rules.members_edit || self.role(who) >= ROLE_ADMIN
    }

    pub fn may_send(&self, who: &[u8; 32]) -> bool {
        self.rules.members_send || self.role(who) >= ROLE_ADMIN
    }

    fn drop_role(&mut self, who: &[u8; 32]) {
        self.owners.retain(|o| o != who);
        self.admins.retain(|a| a != who);
    }

    fn check(&self, roster: &[[u8; 32]]) -> Result<()> {
        let ranked = || self.owners.iter().chain(&self.admins);
        if self.owners.is_empty() {
            return Err("a group needs an owner");
        }
        if ranked().any(|m| !roster.contains(m)) {
            return Err("a role is held by someone outside the group");
        }
        let mut seen = std::collections::HashSet::new();
        if !ranked().all(|m| seen.insert(*m)) {
            return Err("someone holds two roles");
        }
        if !seen.contains(&self.committer) {
            return Err("the committer must be an admin");
        }
        Ok(())
    }
}

/// The state `signed` leaves the group in, when `author` commits it over
/// `state` with `roster` in the group, or why it may not happen. Checks only
/// who may ask for what; that the signature holds and the commit's proposals
/// match the change are the caller's to check.
pub fn apply(
    state: &GroupState, roster: &[[u8; 32]], author: &[u8; 32], signed: &SignedChange,
) -> Result<GroupState> {
    let by = signed.by.0;
    if !roster.contains(&by) {
        return Err("asked for by someone outside the group");
    }
    let carries_committer_request = matches!(&signed.change, GroupChange::MemberRequest(r)
        if r.who.0 == state.committer
            && *author == by && (state.role(author) >= ROLE_ADMIN || state.owners.len() + state.admins.len() == 1));
    if *author != state.committer
        && signed.change != GroupChange::Takeover
        && !carries_committer_request
    {
        return Err("only the committer changes the group");
    }
    let rank = state.role(&by);
    let mut next = state.clone();
    let mut after = roster.to_vec();
    match &signed.change {
        GroupChange::Add { who } => {
            if !state.may_add(&by) {
                return Err("only admins may add people");
            }
            if who.is_empty() {
                return Err("adds nobody");
            }
            for w in who {
                if after.contains(&w.0) {
                    return Err("adds someone already in the group");
                }
                after.push(w.0);
            }
        },
        GroupChange::Remove { who } => {
            let who = who.0;
            if who == by || !roster.contains(&who) {
                return Err("removes nobody who can be removed");
            }
            if rank < ROLE_ADMIN || (state.role(&who) == ROLE_OWNER && rank < ROLE_OWNER) {
                return Err("may not remove them");
            }
            next.drop_role(&who);
            after.retain(|m| *m != who);
        },
        GroupChange::Leave { successor } => {
            next.drop_role(&by);
            after.retain(|m| *m != by);
            if next.owners.is_empty() {
                let s = successor.as_ref().ok_or("the last owner leaves without a successor")?.0;
                if !after.contains(&s) {
                    return Err("the successor isn't in the group");
                }
                next.drop_role(&s);
                next.owners.push(s);
            }
        },
        GroupChange::Role { who, role } => {
            let (who, to) = (who.0, *role);
            let from = state.role(&who);
            if who == by || !roster.contains(&who) || to > ROLE_OWNER || from == to {
                return Err("no such role change");
            }
            let owners_only = from == ROLE_OWNER || to == ROLE_OWNER;
            let appoints = rank == ROLE_ADMIN && state.rules.admins_appoint && !owners_only;
            if rank != ROLE_OWNER && !appoints {
                return Err("may not change their role");
            }
            next.drop_role(&who);
            match to {
                ROLE_OWNER => next.owners.push(who),
                ROLE_ADMIN => next.admins.push(who),
                _ => {},
            }
            // A committer who is no longer an admin can't commit; whoever
            // demoted them carries on.
            if who == state.committer && to == ROLE_MEMBER {
                next.committer = by;
            }
        },
        GroupChange::Rules(rules) => {
            if rank < ROLE_ADMIN || *rules == state.rules {
                return Err("may not change the rules");
            }
            if rules.admins_appoint != state.rules.admins_appoint && rank < ROLE_OWNER {
                return Err("only owners decide who appoints admins");
            }
            next.rules = *rules;
        },
        GroupChange::Handover { to } => {
            let to = to.0;
            if by != state.committer || to == by || !roster.contains(&to) {
                return Err("no such handover");
            }
            // Handing over to a member makes them an admin, but only when
            // there is no admin to hand over to.
            if state.role(&to) == ROLE_MEMBER {
                if state.owners.len() + state.admins.len() > 1 {
                    return Err("hand over to an admin");
                }
                next.admins.push(to);
            }
            next.committer = to;
        },
        GroupChange::Takeover => {
            if by != *author || by == state.committer || rank < ROLE_ADMIN {
                return Err("only an admin takes over");
            }
            next.committer = by;
        },
        GroupChange::Upgrade => return Err("the group already has signed rules"),
        GroupChange::MemberRequest(request) => {
            if !roster.contains(&request.who.0) || request.who.0 == *author {
                return Err("only another current member can carry this request");
            }
            if request.action == common::proto::mls_wire::GroupMemberAction::Leave {
                next.drop_role(&request.who.0);
                after.retain(|m| *m != request.who.0);
                if next.owners.is_empty() {
                    next.drop_role(author);
                    next.owners.push(*author);
                }
            }
            // A committer that lost its keys cannot carry its own refresh.
            // Its identity signature authorizes another eligible member to
            // restore it and carry subsequent changes.
            if state.committer == request.who.0 {
                next.committer = *author;
                if next.role(author) == ROLE_MEMBER {
                    next.admins.push(*author);
                }
            }
        },
    }
    next.last = Some(signed.clone());
    next.check(&after)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use common::types::bytes::Bytes;

    use super::*;

    const A: [u8; 32] = [0xA1; 32];
    const B: [u8; 32] = [0xB1; 32];
    const C: [u8; 32] = [0xC1; 32];
    const D: [u8; 32] = [0xD1; 32];

    fn signed(by: [u8; 32], change: GroupChange) -> SignedChange {
        SignedChange {
            by: by.into(),
            epoch: 1,
            branch: [0; 32].into(),
            change,
            sig: Bytes([0; 64]),
        }
    }

    /// A owns and commits, B is an admin, C and D are members.
    fn group() -> (GroupState, Vec<[u8; 32]>) {
        let mut s = GroupState::founded(A);
        s.admins.push(B);
        (s, vec![A, B, C, D])
    }

    #[test]
    fn roles_decide_who_may_ask_for_what() {
        let (mut s, roster) = group();
        let ask = |s: &GroupState, by, change| apply(s, &roster, &A, &signed(by, change));
        assert!(
            ask(&s, C, GroupChange::Remove { who: D.into() }).is_err(),
            "a member removes no one"
        );
        assert!(ask(&s, B, GroupChange::Remove { who: A.into() }).is_err(), "admins keep owners");
        let appoint = |who: [u8; 32], role| GroupChange::Role { who: who.into(), role };
        assert!(ask(&s, B, appoint(C, ROLE_ADMIN)).is_err(), "admins don't appoint by default");
        let rules = GroupRules { admins_appoint: true, ..s.rules };
        assert!(ask(&s, B, GroupChange::Rules(rules)).is_err(), "only owners let admins appoint");

        s = ask(&s, A, GroupChange::Rules(rules)).unwrap();
        s = ask(&s, B, appoint(C, ROLE_ADMIN)).unwrap();
        assert_eq!(s.role(&C), ROLE_ADMIN);
        assert!(ask(&s, B, appoint(D, ROLE_OWNER)).is_err(), "only owners make owners");

        let quiet = GroupRules { members_send: false, members_add: false, ..s.rules };
        s = ask(&s, B, GroupChange::Rules(quiet)).unwrap();
        assert!(!s.may_send(&D) && s.may_send(&C) && !s.may_add(&D));
        assert!(ask(&s, D, GroupChange::Add { who: vec![[9; 32].into()] }).is_err());
    }

    #[test]
    fn only_the_committer_commits_but_an_admin_may_take_over() {
        let (s, roster) = group();
        let add = signed(C, GroupChange::Add { who: vec![[9; 32].into()] });
        assert!(apply(&s, &roster, &A, &add).is_ok());
        assert!(apply(&s, &roster, &B, &add).is_err(), "B isn't the committer");
        let takeover = |by| signed(by, GroupChange::Takeover);
        assert!(apply(&s, &roster, &C, &takeover(C)).is_err(), "a member can't");
        assert!(apply(&s, &roster, &A, &takeover(B)).is_err(), "B takes over itself");
        let s = apply(&s, &roster, &B, &takeover(B)).unwrap();
        assert_eq!(s.committer, B);
        assert!(apply(&s, &roster, &B, &add).is_ok());
    }

    #[test]
    fn a_group_keeps_an_owner_and_a_committer() {
        let (s, roster) = group();
        let left: Vec<_> = roster.iter().copied().filter(|m| *m != A).collect();
        // A commits, so A hands over before leaving.
        let s = apply(&s, &roster, &A, &signed(A, GroupChange::Handover { to: B.into() })).unwrap();
        let leave = |successor: Option<[u8; 32]>| {
            signed(A, GroupChange::Leave { successor: successor.map(Into::into) })
        };
        assert!(apply(&s, &roster, &B, &leave(None)).is_err(), "the last owner names a successor");
        let s = apply(&s, &roster, &B, &leave(Some(C))).unwrap();
        assert_eq!((s.owners.clone(), s.committer), (vec![C], B));
        assert!(s.check(&left).is_ok());

        // The last owner can't be demoted, and a demoted committer hands on.
        let demote = |by, who: [u8; 32]| {
            signed(by, GroupChange::Role { who: who.into(), role: ROLE_MEMBER })
        };
        assert!(apply(&s, &left, &B, &demote(B, C)).is_err());
        let s = apply(&s, &left, &B, &demote(C, B)).unwrap();
        assert_eq!(s.committer, C);
    }
}
