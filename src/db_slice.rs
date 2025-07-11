// #[sides(client)]

use time::OffsetDateTime;
use liquemap::LiqueMap;
use uuid::Uuid;

use crate::crosstyping::*;


#[cfg(not(feature = "graphics_wasm"))]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_local().unwrap().replace_nanosecond(0).unwrap()
}
#[cfg(feature = "graphics_wasm")]
pub fn now() -> OffsetDateTime {
    js_sys::Date::new_0().into()
}



#[derive(Clone, Copy)]
pub enum MayLoad<'a> {
    Confirmed(&'a Expense),
    Provisional { data: &'a ClientData, temp_time: OffsetDateTime },
    NotLoaded,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordViewKey {
    Confirmed(OffsetDateTime, Uuid),
    Provisional(Uuid)
}
pub enum RecordViewValue {
    Confirmed(Expense),
    Provisional(ClientData, OffsetDateTime)
}
impl RecordViewValue {
    fn borrow(&self) -> MayLoad<'_> {
        use RecordViewValue::*;
        match self {
            Confirmed(c) => MayLoad::Confirmed(c),
            Provisional(data, temp_time) => MayLoad::Provisional{data, temp_time: *temp_time},
        }
    }
}


pub struct DbView<U: Upstream> {
    upstream: U,
    live_records: LiqueMap<RecordViewKey, RecordViewValue>,
    life_stats: CachedStats,
    month_stats: CachedStats
}

impl<U: Upstream> DbView<U> {
    pub fn with(mut upstream: U) -> Self {
        let (life_stats, month_stats, live_records) = upstream.take_init().unwrap_or_default();
        
        let mut live_records_map = LiqueMap::new();
        for exp in live_records {
            live_records_map.insert(
                RecordViewKey::Confirmed(exp.server.time, exp.server.uid),
                RecordViewValue::Confirmed(exp));
        }
        
        Self {
            upstream,
            live_records: live_records_map,
            life_stats,
            month_stats,
        }
    }

    fn keep_month(&mut self) -> OffsetDateTime {
        let liveline = now() - MONTH_LIKE;
        
        while self.month_stats.records_alive > 0 {
            let expense_bottom_index = self.live_records.len() - self.month_stats.records_alive;
            let (_, expense) = self.live_records
                .get_index(expense_bottom_index)
                .expect("we must know all expenses of past month, if only to unbuffer them");
            
            match expense {
                RecordViewValue::Confirmed(expense) if expense.server.time < liveline => {
                    self.month_stats.sub(&expense)
                },
                RecordViewValue::Provisional(client_data, time) if time < &liveline => {
                    let group = client_data.group.as_deref().unwrap_or(UNCLASSIFIED);
                    let amount = client_data.amount as i64;
                    self.month_stats.raw_add(group, -amount, -1);
                },
                _ => break,
            }
        }
        
        liveline
    }

    fn sync_upstream(&mut self) {
        let liveline = self.keep_month();
        for msg in self.upstream.sync() {
            match msg {
                ClientboundUpdate::Revoked { expense } => {
                    self.handle_revocation(expense, liveline);
                }
                ClientboundUpdate::NewSpending { expense, temp_alias } => {
                    self.apply_confirmed(expense, temp_alias, liveline);
                },
                ClientboundUpdate::InitStats { .. } => {
                    // this message is not meant to us; we might do sanity checks, though
                },
                ClientboundUpdate::RevealHistory { expenses } => {
                    for exp in expenses {
                        self.live_records.insert(
                            RecordViewKey::Confirmed(exp.server.time, exp.server.uid),
                            RecordViewValue::Confirmed(exp));
                    }
                    // No stats change because those expenses were already accounted for.
                }
            }
        }
    }

    fn handle_revocation(&mut self, expense: Expense, liveline: OffsetDateTime) {
        let had_known_record = self.live_records.remove(
            &RecordViewKey::Confirmed(expense.server.time, expense.server.uid)
        );
        self.life_stats.sub(&expense);
        if expense.server.time >= liveline {
            debug_assert!(had_known_record.is_some(), "server must have sent this record to us");
            self.month_stats.sub(&expense);
        }
    }

    fn apply_confirmed(&mut self, expense: Expense, temp_alias: Uuid, liveline: OffsetDateTime) {
        assert!(!expense.client.revoked);
        
        let remove_pos = RecordViewKey::Provisional(temp_alias);
        let was_foreign = self.live_records.remove(&remove_pos).is_none();
        if was_foreign {
            self.life_stats.add(&expense);
            if expense.server.time >= liveline {
                self.month_stats.add(&expense);
            }
        }

        let insert_pos = RecordViewKey::Confirmed(expense.server.time, expense.server.uid);
        self.live_records.insert(insert_pos, RecordViewValue::Confirmed(expense));
    }

    pub fn month_transactions_info(&mut self) -> (u64, usize) {
        self.sync_upstream();
        (self.month_stats.total_spending, self.month_stats.records_alive)
    }

    pub fn life_transactions_info(&mut self) -> (u64, usize) {
        self.sync_upstream();
        (self.life_stats.total_spending, self.life_stats.records_alive)
    }
    pub fn total_live_transactions(&mut self) -> usize {
        self.sync_upstream();
        self.life_stats.records_alive
    }

    pub fn month_pie(&mut self) -> &[(String, u64)] {
        self.sync_upstream();
        self.month_stats.group_spendings.as_slice()
    }

    pub fn life_pie(&mut self) -> &[(String, u64)] {
        self.sync_upstream();
        self.life_stats.group_spendings.as_slice()
    }

    pub fn load_last_spendings(&mut self, n: usize) -> impl Iterator<Item = MayLoad<'_>> {
        self.sync_upstream();
        
        // iterators are unfortunately not reversible yet
        // TODO: fix liquemap crate
        
        let total_records = self.life_stats.records_alive;
        let have_records = self.live_records.len();
        
        let visible = self.live_records
            .range_mut_idx(have_records.saturating_sub(n)..)
            .rev()
            .map(|(_k, r)| r.borrow());
        let missing = std::iter::repeat(MayLoad::NotLoaded)
            .take(total_records.saturating_sub(have_records));
        
        visible.chain(missing).take(n)
    }

    pub fn load_some_spendings(&mut self, rev_from: usize, rev_to: usize) -> impl Iterator<Item = MayLoad<'_>> {
        self.sync_upstream();
        
        let total_records = self.life_stats.records_alive;
        let have_records = self.live_records.len();
        
        let visible = self.live_records
            .range_mut_idx(have_records.saturating_sub(rev_to)..have_records.saturating_sub(rev_from))
            .rev()
            .map(|(_k, r)| r.borrow());
        let missing = std::iter::repeat(MayLoad::NotLoaded)
            .take(total_records.saturating_sub(have_records));
        
        visible.chain(missing).take(rev_to - rev_from)
    }

    pub fn insert_expense(&mut self, c: ClientData) {
        assert!(!c.revoked);
        
        let t = now();
        // let temp_alias = Uuid::now_v7();  <- no time facilities on WASM
        let timestamp = uuid::Timestamp::from_unix_time(
            t.unix_timestamp() as u64,
            t.nanosecond(),
            self.live_records.len() as u128,
            20,
        );
        let temp_alias = Uuid::new_v7(timestamp);
        
        self.life_stats.raw_add(c.group.as_deref().unwrap_or(UNCLASSIFIED), c.amount as i64, 1);
        self.month_stats.raw_add(c.group.as_deref().unwrap_or(UNCLASSIFIED), c.amount as i64, 1);
        
        self.live_records.insert(RecordViewKey::Provisional(temp_alias), RecordViewValue::Provisional(c.clone(), t));
        self.upstream.submit(ServerboundUpdate::MadeExpense {
            info: c,
            temp_alias,
        });
    }
}

