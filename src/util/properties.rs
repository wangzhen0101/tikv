// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp;
use std::collections::{HashMap, BTreeMap};
use std::u64;

use storage::mvcc::{Write, WriteType};
use storage::types;
use raftstore::store::keys;
use rocksdb::{DBEntryType, UserCollectedProperties, TablePropertiesCollector,
              TablePropertiesCollectorFactory};
use util::codec;
use util::codec::number::{NumberEncoder, NumberDecoder};

#[derive(Clone, Debug, Default)]
pub struct GetPropertiesOptions {
    pub flags: PropertiesFlags,
    pub max_ts: Option<u64>,
}

bitflags! {
    #[derive(Default)]
    pub struct PropertiesFlags: u32 {
        const MVCC_PROPERTIES       = 0b00000001;
    }
}

const PROP_NUM_ERRORS: &'static str = "tikv.num_errors";
const PROP_MIN_TS: &'static str = "tikv.min_ts";
const PROP_MAX_TS: &'static str = "tikv.max_ts";
const PROP_NUM_ROWS: &'static str = "tikv.num_rows";
const PROP_NUM_PUTS: &'static str = "tikv.num_puts";
const PROP_NUM_VERSIONS: &'static str = "tikv.num_versions";
const PROP_MAX_ROW_VERSIONS: &'static str = "tikv.max_row_versions";

#[derive(Default)]
pub struct UserProperties {
    pub num_errors: u64,
    pub mvcc: Option<MvccProperties>,
}

#[derive(Clone, Debug, Default)]
pub struct MvccProperties {
    pub min_ts: u64, // The minimal timestamp.
    pub max_ts: u64, // The maximal timestamp.
    pub num_rows: u64, // The number of rows.
    pub num_puts: u64, // The number of MVCC puts of all rows.
    pub num_versions: u64, // The number of MVCC versions of all rows.
    pub max_row_versions: u64, // The maximal number of MVCC versions of a single row.
}

impl MvccProperties {
    pub fn new() -> MvccProperties {
        MvccProperties {
            min_ts: u64::MAX,
            max_ts: u64::MIN,
            num_rows: 0,
            num_puts: 0,
            num_versions: 0,
            max_row_versions: 0,
        }
    }

    pub fn add(&mut self, other: &MvccProperties) {
        self.min_ts = cmp::min(self.min_ts, other.min_ts);
        self.max_ts = cmp::max(self.max_ts, other.max_ts);
        self.num_rows += other.num_rows;
        self.num_puts += other.num_puts;
        self.num_versions += other.num_versions;
        self.max_row_versions = cmp::max(self.max_row_versions, other.max_row_versions);
    }

    pub fn encode(&self) -> HashMap<Vec<u8>, Vec<u8>> {
        let items = [(PROP_MIN_TS, self.min_ts),
                     (PROP_MAX_TS, self.max_ts),
                     (PROP_NUM_ROWS, self.num_rows),
                     (PROP_NUM_PUTS, self.num_puts),
                     (PROP_NUM_VERSIONS, self.num_versions),
                     (PROP_MAX_ROW_VERSIONS, self.max_row_versions)];
        items.iter()
            .map(|&(k, v)| {
                let mut buf = Vec::with_capacity(8);
                buf.encode_u64(v).unwrap();
                (k.as_bytes().to_owned(), buf)
            })
            .collect()
    }

    pub fn decode<T: DecodeU64>(props: &T) -> Result<MvccProperties, codec::Error> {
        let mut res = MvccProperties::new();
        res.min_ts = try!(props.decode_u64(PROP_MIN_TS));
        res.max_ts = try!(props.decode_u64(PROP_MAX_TS));
        res.num_rows = try!(props.decode_u64(PROP_NUM_ROWS));
        res.num_puts = try!(props.decode_u64(PROP_NUM_PUTS));
        res.num_versions = try!(props.decode_u64(PROP_NUM_VERSIONS));
        res.max_row_versions = try!(props.decode_u64(PROP_MAX_ROW_VERSIONS));
        Ok(res)
    }
}

pub trait DecodeU64 {
    fn decode_u64(&self, k: &str) -> Result<u64, codec::Error>;
}

impl DecodeU64 for HashMap<Vec<u8>, Vec<u8>> {
    fn decode_u64(&self, k: &str) -> Result<u64, codec::Error> {
        match self.get(k.as_bytes()) {
            Some(v) => v.as_slice().decode_u64(),
            None => Err(codec::Error::KeyNotFound),
        }
    }
}

impl DecodeU64 for UserCollectedProperties {
    fn decode_u64(&self, k: &str) -> Result<u64, codec::Error> {
        match self.get(k.as_bytes()) {
            Some(mut v) => v.decode_u64(),
            None => Err(codec::Error::KeyNotFound),
        }
    }
}

pub struct UserPropertiesCollector {
    mvcc: MvccProperties,
    last_key: Vec<u8>,
    num_errors: u64,
    row_versions: u64,
}

impl UserPropertiesCollector {
    fn new(flags: PropertiesFlags) -> UserPropertiesCollector {
        UserPropertiesCollector {
            mvcc: MvccProperties::new(),
            last_key: Vec::new(),
            num_errors: 0,
            row_versions: 0,
        }
    }

    fn collect_mvcc_properties(&mut self, key: &[u8], value: &[u8], entry_type: DBEntryType) {
        let (k, ts) = match types::split_encoded_key_on_ts(key) {
            Ok((k, ts)) => (k, ts),
            Err(_) => {
                self.num_errors += 1;
                return;
            }
        };

        self.mvcc.min_ts = cmp::min(self.mvcc.min_ts, ts);
        self.mvcc.max_ts = cmp::max(self.mvcc.max_ts, ts);
        match entry_type {
            DBEntryType::Put => self.mvcc.num_versions += 1,
            _ => return,
        }

        if !self.last_key.as_slice().starts_with(k) {
            self.mvcc.num_rows += 1;
            self.row_versions = 1;
        } else {
            self.row_versions += 1;
        }
        if self.row_versions > self.mvcc.max_row_versions {
            self.mvcc.max_row_versions = self.row_versions;
        }

        let v = match Write::parse(value) {
            Ok(v) => v,
            Err(_) => {
                self.num_errors += 1;
                return;
            }
        };

        if v.write_type == WriteType::Put {
            self.mvcc.num_puts += 1;
        }
    }
}

impl TablePropertiesCollector for UserPropertiesCollector {
    fn add(&mut self, key: &[u8], value: &[u8], entry_type: DBEntryType, _: u64, _: u64) {
        if !keys::validate_data_key(key) {
            self.num_errors += 1;
            return;
        }
        self.collect_mvcc_properties(key, value, entry_type);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
    }

    fn finish(&mut self) -> HashMap<Vec<u8>, Vec<u8>> {
        self.mvcc.encode()
    }
}

pub struct UserPropertiesCollectorFactory {
    flags: PropertiesFlags,
}

impl UserPropertiesCollectorFactory {
    pub fn new(flags: PropertiesFlags) -> UserPropertiesCollectorFactory {
        UserPropertiesCollectorFactory { flags: flags }
    }
}

impl TablePropertiesCollectorFactory for UserPropertiesCollectorFactory {
    fn create_table_properties_collector(&mut self, _: u32) -> Box<TablePropertiesCollector> {
        Box::new(UserPropertiesCollector::new(self.flags))
    }
}

#[cfg(test)]
mod tests {
    use rocksdb::{DBEntryType, TablePropertiesCollector};
    use storage::Key;
    use storage::mvcc::{Write, WriteType};
    use raftstore::store::keys;
    use super::{UserPropertiesCollector, MvccProperties, MVCC_PROPERTIES};

    #[test]
    fn test_mvcc_properties_collector() {
        let cases = [("ab", 2, WriteType::Put, DBEntryType::Put),
                     ("ab", 1, WriteType::Delete, DBEntryType::Put),
                     ("ab", 1, WriteType::Delete, DBEntryType::Delete),
                     ("cd", 5, WriteType::Delete, DBEntryType::Put),
                     ("cd", 4, WriteType::Put, DBEntryType::Put),
                     ("cd", 3, WriteType::Put, DBEntryType::Put),
                     ("ef", 6, WriteType::Put, DBEntryType::Put),
                     ("ef", 6, WriteType::Put, DBEntryType::Delete),
                     ("gh", 7, WriteType::Delete, DBEntryType::Put)];
        let mut collector = UserPropertiesCollector::new(MVCC_PROPERTIES);
        for &(key, ts, write_type, entry_type) in &cases {
            let k = Key::from_raw(key.as_bytes()).append_ts(ts);
            let k = keys::data_key(k.encoded());
            let v = Write::new(write_type, ts, None).to_bytes();
            collector.add(&k, &v, entry_type, 0, 0);
        }

        let props = MvccProperties::decode(&collector.finish()).unwrap();
        assert_eq!(props.min_ts, 1);
        assert_eq!(props.max_ts, 7);
        assert_eq!(props.num_rows, 4);
        assert_eq!(props.num_puts, 4);
        assert_eq!(props.num_versions, 7);
        assert_eq!(props.max_row_versions, 3);
    }
}
