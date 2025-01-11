use anyhow::Result;
use link_aggregator::{ActionableEvent, Did, RecordId};
use links::CollectedLink;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

/// consumer-side storage api, independent of actual storage backend
pub trait LinkStorage: StorageBackend {
    fn push(&self, event: &ActionableEvent) -> Result<()> {
        match event {
            ActionableEvent::CreateLinks { record_id, links } => self.add_links(record_id, links),
            ActionableEvent::UpdateLinks {
                record_id,
                new_links,
            } => self.update_links(record_id, new_links),
            ActionableEvent::DeleteRecord(record_id) => self.remove_links(record_id),
            ActionableEvent::ActivateAccount(did) => self.set_account(did, true),
            ActionableEvent::DeactivateAccount(did) => self.set_account(did, false),
            ActionableEvent::DeleteAccount(did) => self.delete_account(did),
        }
        Ok(())
    }
    fn get_count(&self, target: &str, collection: &str, path: &str) -> Result<u64> {
        self.count(target, collection, path)
    }
}

/// persistent data stores
pub trait StorageBackend {
    fn add_links(&self, record_id: &RecordId, links: &[CollectedLink]);
    fn remove_links(&self, record_id: &RecordId);
    fn update_links(&self, record_id: &RecordId, new_links: &[CollectedLink]) {
        self.remove_links(record_id);
        self.add_links(record_id, new_links);
    }
    fn set_account(&self, did: &Did, active: bool);
    fn delete_account(&self, did: &Did);

    fn count(&self, target: &str, collection: &str, path: &str) -> Result<u64>;
}

// hopefully-correct simple hashmap version, intended only for tests to verify disk impl
#[derive(Debug, Clone)]
pub struct MemStorage(Arc<Mutex<MemStorageData>>);

#[derive(Debug, PartialEq, Hash, Eq, Clone)]
struct Target(String);

impl Target {
    fn new(t: &str) -> Self {
        Self(t.into())
    }
}

#[derive(Debug, PartialEq, Hash, Eq, Clone)]
struct Source {
    collection: String,
    path: String,
}

impl Source {
    fn new(collection: &str, path: &str) -> Self {
        Self {
            collection: collection.into(),
            path: path.into(),
        }
    }
}

#[derive(Debug, PartialEq, Hash, Eq, Clone)]
struct RepoId {
    collection: String,
    rkey: String,
}

impl RepoId {
    fn from_record_id(record_id: &RecordId) -> Self {
        Self {
            collection: record_id.collection.clone(),
            rkey: record_id.rkey.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Hash, Eq, Clone)]
struct RecordPath(String);

impl RecordPath {
    fn new(rp: &str) -> Self {
        Self(rp.into())
    }
}

#[derive(Debug, Default)]
struct MemStorageData {
    dids: HashMap<Did, bool>,                            // bool: active or nah
    targets: HashMap<Target, HashMap<Source, Vec<Did>>>, // target -> (collection, path) -> did[]
    links: HashMap<Did, HashMap<RepoId, Vec<(RecordPath, Target)>>>, // did -> collection:rkey -> (path, target)[]
}

impl MemStorage {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(MemStorageData::default())))
    }

    pub fn summarize(&self, qsize: u32) {
        let data = self.0.lock().unwrap();
        let dids = data.dids.len();
        let targets = data.targets.len();
        let target_paths: usize = data.targets.values().map(|paths| paths.len()).sum();
        let links = data.links.len();

        let sample_target = data.targets.keys().nth(data.targets.len() / 2);
        let sample_path = sample_target.and_then(|t| data.targets.get(t).unwrap().keys().next());
        println!("queue: {qsize}. {dids} dids, {targets} targets from {target_paths} paths, {links} links. sample: {sample_target:?} {sample_path:?}");
    }
}

impl LinkStorage for MemStorage {} // defaults are fine

impl StorageBackend for MemStorage {
    fn add_links(&self, record_id: &RecordId, links: &[CollectedLink]) {
        let mut data = self.0.lock().unwrap();
        for link in links {
            data.dids.entry(record_id.did()).or_insert(true); // if they are inserting a link, presumably they are active
            data.targets
                .entry(Target::new(&link.target))
                .or_default()
                .entry(Source::new(&record_id.collection, &link.path))
                .or_default()
                .push(record_id.did());
            data.links
                .entry(record_id.did())
                .or_default()
                .entry(RepoId::from_record_id(record_id))
                .or_insert(Vec::with_capacity(1))
                .push((RecordPath::new(&link.path), Target::new(&link.target)))
        }
    }

    fn set_account(&self, did: &Did, active: bool) {
        let mut data = self.0.lock().unwrap();
        if let Some(account) = data.dids.get_mut(did) {
            *account = active;
        }
    }

    fn remove_links(&self, record_id: &RecordId) {
        let mut data = self.0.lock().unwrap();
        let repo_id = RepoId::from_record_id(record_id);
        if let Some(Some(link_targets)) = data.links.get(&record_id.did).map(|cr| cr.get(&repo_id))
        {
            let link_targets = link_targets.clone(); // satisfy borrowck
            for (record_path, target) in link_targets {
                let dids = data
                    .targets
                    .get_mut(&target)
                    .expect("must have the target if we have a link saved")
                    .get_mut(&Source::new(&record_id.collection, &record_path.0))
                    .expect("must have the target at this path if we have a link to it saved");
                // search from the end: more likely to be visible and deletes are usually soon after creates
                // only delete one instance: a user can create multiple links to something, we're only deleting one
                // (we don't know which one in the list we should be deleting, and it hopefully mostly doesn't matter)
                let pos = dids
                    .iter()
                    .rposition(|d| *d == record_id.did)
                    .expect("must be in dids list if we have a link to it");
                dids.remove(pos);
            }
        }
        data.links
            .get_mut(&record_id.did)
            .map(|cr| cr.remove(&repo_id));
    }

    fn delete_account(&self, did: &Did) {
        let mut data = self.0.lock().unwrap();
        if let Some(links) = data.links.get(did) {
            let links = links.clone();
            for (repo_id, targets) in links {
                let targets = targets.clone();
                for (record_path, target) in targets {
                    data.targets
                        .get_mut(&target)
                        .expect("must have the target if we have a link saved")
                        .get_mut(&Source::new(&repo_id.collection, &record_path.0))
                        .expect("must have the target at this path if we have a link to it saved")
                        .retain(|d| d != did);
                }
            }
        }
        data.links.remove(did);
        data.dids.remove(did);
    }

    fn count(&self, target: &str, collection: &str, path: &str) -> Result<u64> {
        let data = self.0.lock().unwrap();
        let Some(paths) = data.targets.get(&Target::new(target)) else {
            return Ok(0);
        };
        let Some(dids) = paths.get(&Source::new(collection, path)) else {
            return Ok(0);
        };
        let count = dids.len().try_into()?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use links::CollectedLink;

    #[test]
    fn test_mem_empty() {
        let storage = MemStorage::new();
        assert_eq!(storage.get_count("", "", "").unwrap(), 0);
        assert_eq!(storage.get_count("a", "b", "c").unwrap(), 0);
        assert_eq!(
            storage
                .get_count(
                    "at://did:plc:b3rzzkblqsxhr3dgcueymkqe/app.bsky.feed.post/3lf6yc4drhk2f",
                    "app.test.collection",
                    ".reply.parent.uri"
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_mem_links() {
        let storage = MemStorage::new();
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "fdsa".into(),
                },
                links: vec![CollectedLink {
                    target: "e.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("bad.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".def.uri")
                .unwrap(),
            0
        );

        // delete under the wrong collection
        storage
            .push(&ActionableEvent::DeleteRecord(RecordId {
                did: "did:plc:asdf".into(),
                collection: "app.test.wrongcollection".into(),
                rkey: "fdsa".into(),
            }))
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );

        // delete under the wrong rkey
        storage
            .push(&ActionableEvent::DeleteRecord(RecordId {
                did: "did:plc:asdf".into(),
                collection: "app.test.collection".into(),
                rkey: "wrongkey".into(),
            }))
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );

        // finally actually delete it
        storage
            .push(&ActionableEvent::DeleteRecord(RecordId {
                did: "did:plc:asdf".into(),
                collection: "app.test.collection".into(),
                rkey: "fdsa".into(),
            }))
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            0
        );

        // put it back
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "fdsa".into(),
                },
                links: vec![CollectedLink {
                    target: "e.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );

        // add another link from this user
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "fdsa2".into(),
                },
                links: vec![CollectedLink {
                    target: "e.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            2
        );

        // add a link from someone else
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdfasdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "fdsa".into(),
                },
                links: vec![CollectedLink {
                    target: "e.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            3
        );

        // aaaand delete the first one again
        storage
            .push(&ActionableEvent::DeleteRecord(RecordId {
                did: "did:plc:asdf".into(),
                collection: "app.test.collection".into(),
                rkey: "fdsa".into(),
            }))
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            2
        );
    }

    #[test]
    fn test_mem_two_user_links_delete_one() {
        let storage = MemStorage::new();

        // create the first link
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "A".into(),
                },
                links: vec![CollectedLink {
                    target: "e.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );

        // create the second link (same user, different rkey)
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "B".into(),
                },
                links: vec![CollectedLink {
                    target: "e.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            2
        );

        // aaaand delete the first link
        storage
            .push(&ActionableEvent::DeleteRecord(RecordId {
                did: "did:plc:asdf".into(),
                collection: "app.test.collection".into(),
                rkey: "A".into(),
            }))
            .unwrap();

        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_mem_accounts() {
        let storage = MemStorage::new();

        // create two links
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "A".into(),
                },
                links: vec![CollectedLink {
                    target: "a.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "B".into(),
                },
                links: vec![CollectedLink {
                    target: "b.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("a.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("b.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );

        // and a third from a different account
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:fdsa".into(),
                    collection: "app.test.collection".into(),
                    rkey: "A".into(),
                },
                links: vec![CollectedLink {
                    target: "a.com".into(),
                    path: ".abc.uri".into(),
                }],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("a.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            2
        );

        // delete the first account
        storage
            .push(&ActionableEvent::DeleteAccount("did:plc:asdf".into()))
            .unwrap();
        assert_eq!(
            storage
                .get_count("a.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("b.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_multi_link() {
        let storage = MemStorage::new();
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "fdsa".into(),
                },
                links: vec![
                    CollectedLink {
                        target: "e.com".into(),
                        path: ".abc.uri".into(),
                    },
                    CollectedLink {
                        target: "f.com".into(),
                        path: ".xyz[].uri".into(),
                    },
                    CollectedLink {
                        target: "g.com".into(),
                        path: ".xyz[].uri".into(),
                    },
                ],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("f.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("g.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            1
        );

        storage
            .push(&ActionableEvent::DeleteRecord(RecordId {
                did: "did:plc:asdf".into(),
                collection: "app.test.collection".into(),
                rkey: "fdsa".into(),
            }))
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .get_count("f.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .get_count("g.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_update_link() {
        let storage = MemStorage::new();

        // create the links
        storage
            .push(&ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "fdsa".into(),
                },
                links: vec![
                    CollectedLink {
                        target: "e.com".into(),
                        path: ".abc.uri".into(),
                    },
                    CollectedLink {
                        target: "f.com".into(),
                        path: ".xyz[].uri".into(),
                    },
                    CollectedLink {
                        target: "g.com".into(),
                        path: ".xyz[].uri".into(),
                    },
                ],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("f.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("g.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            1
        );

        // update them
        storage
            .push(&ActionableEvent::UpdateLinks {
                record_id: RecordId {
                    did: "did:plc:asdf".into(),
                    collection: "app.test.collection".into(),
                    rkey: "fdsa".into(),
                },
                new_links: vec![
                    CollectedLink {
                        target: "h.com".into(),
                        path: ".abc.uri".into(),
                    },
                    CollectedLink {
                        target: "f.com".into(),
                        path: ".xyz[].uri".into(),
                    },
                    CollectedLink {
                        target: "i.com".into(),
                        path: ".xyz[].uri".into(),
                    },
                ],
            })
            .unwrap();
        assert_eq!(
            storage
                .get_count("e.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .get_count("h.com", "app.test.collection", ".abc.uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("f.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .get_count("g.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .get_count("i.com", "app.test.collection", ".xyz[].uri")
                .unwrap(),
            1
        );
    }
}