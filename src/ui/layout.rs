//! How the dashboard shares its rows between sections, and what detail
//! each pane size can afford.

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SectionId {
    Changes,
    Activity,
    Commits,
    Branches,
    Stashes,
}

impl SectionId {
    /// Priority order.
    pub const ALL: [SectionId; 5] = [
        SectionId::Changes,
        SectionId::Activity,
        SectionId::Commits,
        SectionId::Branches,
        SectionId::Stashes,
    ];

    pub fn title(self) -> &'static str {
        match self {
            SectionId::Changes => "Changes",
            SectionId::Activity => "Activity",
            SectionId::Commits => "Commits",
            SectionId::Branches => "Branches",
            SectionId::Stashes => "Stashes",
        }
    }

    /// Name used in `config.toml`.
    pub fn key(self) -> &'static str {
        match self {
            SectionId::Changes => "changes",
            SectionId::Activity => "activity",
            SectionId::Commits => "commits",
            SectionId::Branches => "branches",
            SectionId::Stashes => "stashes",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Density {
    pub borders: bool,
    pub stats: bool,
    pub ages: bool,
    pub hints: bool,
    pub tiny: bool,
}

impl Density {
    pub fn for_size(w: u16, h: u16) -> Density {
        Density {
            borders: w >= 36 && h >= 14,
            stats: w >= 36,
            ages: w >= 30,
            hints: h >= 12,
            tiny: w < 24,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SectionReq {
    pub id: SectionId,
    pub folded: bool,
    pub content: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Hidden,
    Collapsed,
    Expanded { rows: usize },
}

/// Shares `avail` rows between sections in priority order. `overhead` is
/// what an expanded section costs beyond its content rows (2 with borders,
/// 1 with a title rule); a collapsed section costs 1 row.
pub fn allocate(avail: usize, overhead: usize, reqs: &[SectionReq]) -> Vec<Slot> {
    if avail < reqs.len() {
        return (0..reqs.len())
            .map(|i| {
                if i < avail {
                    Slot::Collapsed
                } else {
                    Slot::Hidden
                }
            })
            .collect();
    }
    let mut slots = vec![Slot::Collapsed; reqs.len()];
    let mut left = avail - reqs.len();
    for (slot, req) in slots.iter_mut().zip(reqs) {
        if req.folded || req.content == 0 {
            continue;
        }
        let rows = req.content.min(2);
        let extra = overhead + rows - 1;
        if extra <= left {
            left -= extra;
            *slot = Slot::Expanded { rows };
        }
    }
    for (slot, req) in slots.iter_mut().zip(reqs) {
        if let Slot::Expanded { rows } = slot {
            let more = (req.content - *rows).min(left);
            *rows += more;
            left -= more;
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn five(c: [usize; 5]) -> Vec<SectionReq> {
        SectionId::ALL
            .iter()
            .zip(c)
            .map(|(id, content)| SectionReq {
                id: *id,
                folded: false,
                content,
            })
            .collect()
    }

    #[test]
    fn plenty_of_room() {
        let s = allocate(100, 2, &five([5, 3, 10, 4, 1]));
        let rows = |r| Slot::Expanded { rows: r };
        assert_eq!(s, vec![rows(5), rows(3), rows(10), rows(4), rows(1)]);
    }

    #[test]
    fn tight_collapses_low_priority() {
        // 5 collapsed rows + Changes upgrade (3) + Activity upgrade (3) = 11; 1 row left for Changes.
        let s = allocate(12, 2, &five([5, 3, 10, 4, 1]));
        assert_eq!(
            s,
            vec![
                Slot::Expanded { rows: 3 },
                Slot::Expanded { rows: 2 },
                Slot::Collapsed,
                Slot::Collapsed,
                Slot::Collapsed
            ]
        );
    }

    #[test]
    fn fewer_rows_than_sections() {
        assert_eq!(
            allocate(2, 2, &five([1; 5])),
            vec![
                Slot::Collapsed,
                Slot::Collapsed,
                Slot::Hidden,
                Slot::Hidden,
                Slot::Hidden
            ]
        );
        assert_eq!(allocate(0, 2, &five([1; 5])), vec![Slot::Hidden; 5]);
    }

    #[test]
    fn folded_and_empty_stay_collapsed() {
        let mut r = five([5, 3, 3, 3, 0]);
        r[0].folded = true;
        let s = allocate(100, 2, &r);
        assert_eq!(s[0], Slot::Collapsed);
        assert_eq!(s[4], Slot::Collapsed);
        assert_eq!(s[1], Slot::Expanded { rows: 3 });
    }

    #[test]
    fn density_thresholds() {
        let d = Density::for_size(44, 28);
        assert!(d.borders && d.stats && d.ages && d.hints && !d.tiny);
        let d = Density::for_size(30, 12);
        assert!(!d.borders && !d.stats && d.ages && d.hints && !d.tiny);
        let d = Density::for_size(29, 11);
        assert!(!d.ages && !d.hints);
        assert!(!Density::for_size(40, 13).borders);
        assert!(Density::for_size(23, 40).tiny);
    }
}
