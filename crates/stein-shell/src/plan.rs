//! What it takes to make the surfaces a scene has match the ones it wants.
//!
//! Pure, so the rules are tested without a compositor: a surface goes when
//! its key is no longer wanted or its role changed, and a popup goes with
//! its parent. Drops come children first, which is the order xdg-shell
//! demands; makes come in the scene's order, parents first.

use crate::multi::{Role, Want};

/// Fill `drop` with indices into `have` to destroy, highest first, and
/// `make` with indices into `wants` to create, in order.
pub(crate) fn plan(
    have: &[(u32, Role)],
    wants: &[Want],
    drop: &mut Vec<usize>,
    make: &mut Vec<usize>,
) {
    drop.clear();
    make.clear();
    for (i, (key, role)) in have.iter().enumerate() {
        if !wants.iter().any(|w| w.key == *key && w.role == *role) {
            drop.push(i);
        }
    }
    // A popup whose parent goes, or never was, goes too. Repeated until
    // nothing changes, since popups can hang off popups.
    while let Some(i) = (0..have.len()).find(|&i| !drop.contains(&i) && orphan(have, drop, i)) {
        drop.push(i);
    }
    drop.sort_unstable_by(|a, b| b.cmp(a));
    for (j, w) in wants.iter().enumerate() {
        let kept = have
            .iter()
            .enumerate()
            .any(|(i, (key, _))| *key == w.key && !drop.contains(&i));
        if !kept {
            make.push(j);
        }
    }
}

/// Whether `have[i]` is a popup whose parent is missing or going.
fn orphan(have: &[(u32, Role)], drop: &[usize], i: usize) -> bool {
    let Role::Popup(p) = have[i].1 else {
        return false;
    };
    let parent = have.iter().position(|(k, _)| *k == p.parent);
    parent.is_none_or(|j| drop.contains(&j))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::bar;
    use crate::multi::{Popup, Side};

    fn layer(h: u32) -> Role {
        Role::Layer {
            layer: bar(h, false, "t"),
            output: Some(1),
        }
    }

    fn popup(parent: u32) -> Role {
        Role::Popup(Popup {
            parent,
            anchor: [0, 0, 10, 10],
            size: [100, 100],
            side: Side::Below,
            grab: true,
        })
    }

    fn run(have: &[(u32, Role)], wants: &[Want]) -> (Vec<usize>, Vec<usize>) {
        let (mut d, mut m) = (Vec::new(), Vec::new());
        plan(have, wants, &mut d, &mut m);
        (d, m)
    }

    #[test]
    fn nothing_to_do_when_they_match() {
        let have = [(1, layer(24)), (2, popup(1))];
        let wants = [
            Want {
                key: 1,
                role: layer(24),
            },
            Want {
                key: 2,
                role: popup(1),
            },
        ];
        assert_eq!(run(&have, &wants), (vec![], vec![]));
    }

    #[test]
    fn new_keys_are_made_in_order() {
        let wants = [
            Want {
                key: 1,
                role: layer(24),
            },
            Want {
                key: 2,
                role: layer(24),
            },
        ];
        assert_eq!(run(&[], &wants), (vec![], vec![0, 1]));
    }

    #[test]
    fn a_changed_role_is_remade() {
        let have = [(1, layer(24))];
        let wants = [Want {
            key: 1,
            role: layer(30),
        }];
        assert_eq!(run(&have, &wants), (vec![0], vec![0]));
    }

    #[test]
    fn a_popup_goes_with_its_parent_children_first() {
        let have = [(1, layer(24)), (2, popup(1)), (3, popup(2))];
        let wants = [];
        assert_eq!(run(&have, &wants), (vec![2, 1, 0], vec![]));
        // Only the parent unwanted: its whole chain still goes.
        let wants = [
            Want {
                key: 2,
                role: popup(1),
            },
            Want {
                key: 3,
                role: popup(2),
            },
        ];
        let (d, m) = run(&have, &wants);
        assert_eq!(d, vec![2, 1, 0]);
        assert_eq!(
            m,
            vec![0, 1],
            "wanted again, so remade, and skipped until a parent exists"
        );
    }

    #[test]
    fn a_dismissed_popup_alone_goes() {
        let have = [(1, layer(24)), (2, popup(1))];
        let wants = [Want {
            key: 1,
            role: layer(24),
        }];
        assert_eq!(run(&have, &wants), (vec![1], vec![]));
    }
}
