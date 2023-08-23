// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use rand::Rng;
use rkyv::{Archive, Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, VecDeque},
    path::Path,
};
use url::Url;

use super::{Domain, Job, JobResponse, Result, UrlResponse};

const URLS_PER_SHARD: usize = 5_000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Archive)]
#[archive(check_bytes)]
pub enum UrlStatus {
    Pending,
    Crawling,
    Failed { status_code: Option<u16> },
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Archive)]
#[archive(check_bytes)]
pub enum DomainStatus {
    Pending,
    CrawlInProgress,
}

struct SampledItem<T> {
    item: T,
    priority: f64,
}

impl<T> PartialEq for SampledItem<T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl<T> Eq for SampledItem<T> {}

impl<T> PartialOrd for SampledItem<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.priority.partial_cmp(&other.priority)
    }
}

impl<T> Ord for SampledItem<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .partial_cmp(&other.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

fn weighted_sample<T>(items: impl Iterator<Item = (T, f64)>, num_items: usize) -> Vec<T> {
    let mut sampled_items: BinaryHeap<SampledItem<T>> = BinaryHeap::with_capacity(num_items);

    let mut rng = rand::thread_rng();

    for (item, weight) in items {
        // see https://www.kaggle.com/code/kotamori/random-sample-with-weights-on-sql/notebook for details on math
        let priority = -(rng.gen::<f64>().abs() + f64::EPSILON).ln() / (weight + 1.0);

        if sampled_items.len() < num_items {
            sampled_items.push(SampledItem { item, priority });
        } else if let Some(mut max) = sampled_items.peek_mut() {
            if priority < max.priority {
                max.item = item;
                max.priority = priority;
            }
        }
    }

    sampled_items.into_iter().map(|s| s.item).collect()
}

#[derive(Clone, Serialize, Deserialize, Archive)]
#[archive(check_bytes)]
struct UrlState {
    weight: f64,
    status: UrlStatus,
}

#[derive(Clone, Serialize, Deserialize, Archive)]
#[archive(check_bytes)]
struct DomainState {
    weight: f64,
    status: DomainStatus,
    max_shard_id: u64,
}

pub struct RedirectDb {
    inner: rocksdb::DB,
}

impl RedirectDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();

        options.create_if_missing(true);

        let block_options = rocksdb::BlockBasedOptions::default();

        options.set_block_based_table_factory(&block_options);

        let inner = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self { inner })
    }

    pub fn put(&self, from: &Url, to: &Url) -> Result<()> {
        let url_bytes = bincode::serialize(from)?;
        let redirect_bytes = bincode::serialize(to)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.disable_wal(true);
        self.inner
            .put_opt(url_bytes, redirect_bytes, &write_options)?;

        Ok(())
    }

    pub fn get(&self, from: &Url) -> Result<Option<Url>> {
        let url_bytes = bincode::serialize(from)?;
        let redirect_bytes = self.inner.get(url_bytes)?;

        if let Some(redirect_bytes) = redirect_bytes {
            let redirect: Url = bincode::deserialize(&redirect_bytes)?;
            return Ok(Some(redirect));
        }

        Ok(None)
    }
}

#[derive(Clone, Serialize, Deserialize, Archive)]
#[archive(check_bytes)]
struct DomainShard {
    domain: Domain,
    shard_id: u64,
}

struct UrlToInsert {
    url: Url,
    different_domain: bool,
}

struct UrlStateDb {
    db: rocksdb::DB,
}

impl UrlStateDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();

        options.create_if_missing(true);

        let mut block_options = rocksdb::BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);

        options.set_block_based_table_factory(&block_options);
        options.set_optimize_filters_for_hits(true);
        options.set_max_background_jobs(8);
        options.set_write_buffer_size(512 * 1024 * 1024);
        options.set_allow_mmap_reads(true);
        options.set_allow_mmap_writes(true);
        options.optimize_for_point_lookup(512);
        options.set_max_subcompactions(8);
        options.set_compaction_style(rocksdb::DBCompactionStyle::Universal);
        options.set_compression_type(rocksdb::DBCompressionType::None);

        let db = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self { db })
    }

    pub fn get(&mut self, key: &DomainShard) -> Result<Option<BTreeMap<UrlString, UrlState>>> {
        let key_bytes = rkyv::to_bytes::<_, 1024>(key)?;
        let value_bytes = self.db.get(key_bytes)?;

        match value_bytes {
            Some(value_bytes) => {
                let archived =
                    rkyv::check_archived_root::<BTreeMap<UrlString, UrlState>>(&value_bytes[..])
                        .unwrap();
                let value = archived.deserialize(&mut rkyv::Infallible).unwrap();
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub fn put(&mut self, key: &DomainShard, value: &BTreeMap<UrlString, UrlState>) -> Result<()> {
        let key_bytes = rkyv::to_bytes::<_, 1024>(key)?;
        let value_bytes = rkyv::to_bytes::<_, 1024>(value)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.disable_wal(true);
        self.db.put_opt(key_bytes, value_bytes, &write_options)?;

        Ok(())
    }
}

struct DomainStateDb {
    db: rocksdb::DB,
}

impl DomainStateDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();

        options.create_if_missing(true);

        let mut block_options = rocksdb::BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);

        options.set_block_based_table_factory(&block_options);
        options.set_optimize_filters_for_hits(true);
        options.set_max_background_jobs(8);
        options.set_write_buffer_size(512 * 1024 * 1024);
        options.set_allow_mmap_reads(true);
        options.set_allow_mmap_writes(true);
        options.set_max_subcompactions(8);

        let db = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self { db })
    }

    fn get(&self, domain: &Domain) -> Result<Option<DomainState>> {
        let domain_bytes = rkyv::to_bytes::<_, 1024>(domain)?;
        let value_bytes = self.db.get(domain_bytes)?;

        if let Some(value_bytes) = &value_bytes {
            let archived = rkyv::check_archived_root::<DomainState>(&value_bytes[..]).unwrap();
            let value = archived.deserialize(&mut rkyv::Infallible).unwrap();
            return Ok(Some(value));
        }

        Ok(None)
    }

    fn put(&self, domain: &Domain, state: &DomainState) -> Result<()> {
        let domain_bytes = rkyv::to_bytes::<_, 1024>(domain)?;
        let state_bytes = rkyv::to_bytes::<_, 1024>(state)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.disable_wal(true);
        self.db.put_opt(domain_bytes, state_bytes, &write_options)?;

        Ok(())
    }

    fn iter(&self) -> impl Iterator<Item = (Domain, DomainState)> + '_ {
        let iter = self.db.iterator(rocksdb::IteratorMode::Start);

        iter.filter_map(|r| {
            let (key, value) = r.ok()?;
            let domain_archive = rkyv::check_archived_root::<Domain>(&key[..]).ok()?;
            let state_archive = rkyv::check_archived_root::<DomainState>(&value[..]).ok()?;

            let domain: Domain = domain_archive.deserialize(&mut rkyv::Infallible).ok()?;
            let state: DomainState = state_archive.deserialize(&mut rkyv::Infallible).ok()?;

            Some((domain, state))
        })
    }
}

#[derive(Clone, Serialize, Deserialize, Archive, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[archive(check_bytes)]
#[archive_attr(derive(Hash, PartialEq, Eq, PartialOrd, Ord))]
struct UrlString(String);

impl From<&Url> for UrlString {
    fn from(url: &Url) -> Self {
        Self(url.as_str().to_string())
    }
}

impl From<Url> for UrlString {
    fn from(url: Url) -> Self {
        Self(url.as_str().to_string())
    }
}

impl From<&UrlString> for Url {
    fn from(url: &UrlString) -> Self {
        Url::parse(&url.0).unwrap()
    }
}

pub struct CrawlDb {
    domain_state: DomainStateDb,
    urls: UrlStateDb,
    redirects: RedirectDb,
}

impl CrawlDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        if Path::new(path.as_ref()).exists() {
            return Err(anyhow::anyhow!(
                "crawl db already exists and might be in incorrect state"
            ));
        }

        Ok(Self {
            redirects: RedirectDb::open(path.as_ref().join("redirects"))?,
            domain_state: DomainStateDb::open(path.as_ref().join("domains"))?,
            urls: UrlStateDb::open(path.as_ref().join("urls"))?,
        })
    }

    pub fn insert_seed_urls(&mut self, urls: &[Url]) -> Result<()> {
        for url in urls {
            let domain = Domain::from(url);

            self.domain_state.put(
                &domain,
                &DomainState {
                    weight: 0.0,
                    status: DomainStatus::Pending,
                    max_shard_id: 0,
                },
            )?;

            let sharded_domain = DomainShard {
                domain,
                shard_id: 0,
            };

            let mut urls = self.urls.get(&sharded_domain)?.unwrap_or_default();

            urls.insert(
                url.into(),
                UrlState {
                    weight: 0.0,
                    status: UrlStatus::Pending,
                },
            );

            self.urls.put(&sharded_domain, &urls)?;
        }

        Ok(())
    }

    pub fn insert_urls(&mut self, responses: &[JobResponse]) -> Result<()> {
        let mut domains: HashMap<Domain, Vec<UrlToInsert>> = HashMap::new();

        responses.iter().for_each(|res| {
            for url in &res.discovered_urls {
                let domain = Domain::from(url);
                let different_domain = res.domain != domain;

                domains.entry(domain).or_default().push(UrlToInsert {
                    url: url.clone(),
                    different_domain,
                });
            }

            for url_res in &res.url_responses {
                if let UrlResponse::Redirected { url, new_url } = url_res {
                    self.redirects.put(url, new_url).unwrap();
                }
            }
        });

        for (domain, urls) in domains.into_iter() {
            let mut domain_state = match self.domain_state.get(&domain)? {
                Some(state) => state,
                None => {
                    let state = DomainState {
                        weight: 0.0,
                        status: DomainStatus::Pending,
                        max_shard_id: 0,
                    };
                    self.domain_state.put(&domain, &state)?;

                    state
                }
            };

            let mut sharded_domain = DomainShard {
                domain,
                shard_id: domain_state.max_shard_id,
            };

            let mut url_states = self.urls.get(&sharded_domain)?.unwrap_or_default();

            if url_states.len() >= URLS_PER_SHARD {
                domain_state.max_shard_id += 1;
                sharded_domain.shard_id = domain_state.max_shard_id;

                url_states = BTreeMap::new();
            }

            for url in urls {
                let mut url_state = url_states
                    .get(&UrlString::from(&url.url))
                    .cloned()
                    .unwrap_or(UrlState {
                        weight: 0.0,
                        status: UrlStatus::Pending,
                    });

                if url.different_domain {
                    url_state.weight += 1.0;
                }

                if url_state.weight > domain_state.weight {
                    domain_state.weight = url_state.weight;
                }

                url_states.insert(url.url.into(), url_state);
            }

            self.domain_state
                .put(&sharded_domain.domain, &domain_state)?;

            self.urls.put(&sharded_domain, &url_states)?;
        }

        Ok(())
    }

    pub fn set_domain_status(&mut self, domain: &Domain, status: DomainStatus) -> Result<()> {
        let mut domain_state = self.domain_state.get(domain)?.unwrap_or(DomainState {
            weight: 0.0,
            status,
            max_shard_id: 0,
        });

        domain_state.status = status;

        self.domain_state.put(domain, &domain_state)?;

        Ok(())
    }

    pub fn sample_domains(&mut self, num_jobs: usize) -> Result<Vec<Domain>> {
        let sampled = weighted_sample(
            self.domain_state.iter().filter_map(|(domain, state)| {
                if state.status == DomainStatus::Pending {
                    Some((domain, state.weight))
                } else {
                    None
                }
            }),
            num_jobs,
        );

        for domain in sampled.iter() {
            let mut state = self.domain_state.get(domain)?.unwrap();
            state.status = DomainStatus::CrawlInProgress;
            self.domain_state.put(domain, &state)?;
        }

        Ok(sampled)
    }

    pub fn prepare_jobs(&mut self, domains: &[Domain], urls_per_job: usize) -> Result<Vec<Job>> {
        let mut jobs = Vec::with_capacity(domains.len());
        for domain in domains {
            let state = self.domain_state.get(domain)?.unwrap();

            let shard_id = rand::thread_rng().gen_range(0..=state.max_shard_id);

            let shard = DomainShard {
                domain: domain.clone(),
                shard_id,
            };

            let mut urls = self.urls.get(&shard)?.unwrap_or_default();
            let available_urls: Vec<_> = urls
                .iter()
                .filter_map(|(url, state)| {
                    if state.status == UrlStatus::Pending {
                        Some((url.clone(), state.weight))
                    } else {
                        None
                    }
                })
                .collect();

            let sampled: Vec<_> = weighted_sample(
                available_urls.iter().map(|(url, weight)| (url, *weight)),
                urls_per_job,
            );

            for url in &sampled {
                let mut state = urls.get(url).unwrap().clone();
                state.status = UrlStatus::Crawling;

                urls.insert((*url).clone(), state);
            }

            let mut domain_state = self.domain_state.get(domain)?.unwrap();

            domain_state.weight = urls
                .iter()
                .filter_map(|(_, state)| {
                    if state.status == UrlStatus::Pending {
                        Some(state.weight)
                    } else {
                        None
                    }
                })
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                .unwrap_or(0.0);

            self.domain_state.put(domain, &domain_state)?;

            let mut job = Job {
                domain: domain.clone(),
                fetch_sitemap: false, // todo: fetch for new sites
                urls: VecDeque::with_capacity(urls_per_job),
            };

            for url in sampled {
                job.urls.push_back(url.into());
            }

            jobs.push(job);

            self.urls.put(&shard, &urls)?;
        }

        Ok(jobs)
    }
}

#[cfg(test)]
mod tests {
    use crate::gen_temp_path;

    use super::*;

    #[test]
    fn sampling() {
        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 10);
        assert_eq!(sampled.len(), items.len());

        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 1);
        assert_eq!(sampled.len(), 1);

        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 0);
        assert_eq!(sampled.len(), 0);

        let items: Vec<(usize, f64)> = vec![(0, 1000000000.0), (1, 2.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 1);
        assert_eq!(sampled.len(), 1);
        assert_eq!(*sampled[0], 0);
    }

    #[test]
    fn simple_politeness() {
        let mut db = CrawlDb::open(gen_temp_path()).unwrap();

        db.insert_seed_urls(&[Url::parse("https://example.com").unwrap()])
            .unwrap();

        let domain = Domain::from(&Url::parse("https://example.com").unwrap());

        let sample = db.sample_domains(128).unwrap();

        assert_eq!(sample.len(), 1);
        assert_eq!(&sample[0], &domain);
        assert_eq!(
            db.domain_state.get(&domain).unwrap().unwrap().status,
            DomainStatus::CrawlInProgress
        );

        let new_sample = db.sample_domains(128).unwrap();
        assert_eq!(new_sample.len(), 0);
    }
}
