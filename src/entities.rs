use alloc::vec::Vec;
use core::cmp;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicI64, Ordering};
use core::{fmt, mem};
#[cfg(feature = "std")]
use std::error::Error;

/// Lightweight unique ID of an entity
///
/// Obtained from `World::spawn`. Can be stored to refer to an entity in the future.
#[derive(Clone, Copy, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Entity {
    pub(crate) generation: u32,
    pub(crate) id: u32,
}

impl Entity {
    /// Convert to a form convenient for passing outside of rust
    ///
    /// Only useful for identifying entities within the same instance of an application. Do not use
    /// for serialization between runs.
    ///
    /// No particular structure is guaranteed for the returned bits.
    pub fn to_bits(self) -> u64 {
        u64::from(self.generation) << 32 | u64::from(self.id)
    }

    /// Reconstruct an `Entity` previously destructured with `to_bits`
    ///
    /// Only useful when applied to results from `to_bits` in the same instance of an application.
    pub fn from_bits(bits: u64) -> Self {
        Self {
            generation: (bits >> 32) as u32,
            id: bits as u32,
        }
    }

    /// Extract a transiently unique identifier
    ///
    /// No two simultaneously-live entities share the same ID, but dead entities' IDs may collide
    /// with both live and dead entities. Useful for compactly representing entities within a
    /// specific snapshot of the world, such as when serializing.
    pub fn id(self) -> u32 {
        self.id
    }
}

impl fmt::Debug for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}v{}", self.id, self.generation)
    }
}

/// An iterator returning a sequence of Entity values from `Entities::reserve_entities`.
pub struct ReserveEntitiesIterator<'a> {
    // Metas, so we can recover the current generation for anything in the freelist.
    meta: &'a [EntityMeta],

    // Reserved IDs formerly in the freelist to hand out.
    id_iter: core::slice::Iter<'a, u32>,

    // New Entity IDs to hand out, outside the range of meta.len().
    id_range: core::ops::Range<u32>,
}

impl<'a> Iterator for ReserveEntitiesIterator<'a> {
    type Item = Entity;

    fn next(&mut self) -> Option<Self::Item> {
        self.id_iter
            .next()
            .map(|&id| Entity {
                generation: self.meta[id as usize].generation,
                id,
            })
            .or_else(|| self.id_range.next().map(|id| Entity { generation: 0, id }))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.id_iter.len() + self.id_range.len();
        (len, Some(len))
    }
}

impl<'a> core::iter::ExactSizeIterator for ReserveEntitiesIterator<'a> {}

#[derive(Default)]
pub(crate) struct Entities {
    pub meta: Vec<EntityMeta>,

    // The `pending` and `free_cursor` fields describe three sets of Entity IDs
    // that have been freed or are in the process of being allocated:
    //
    // - The `freelist` IDs, previously freed by `free()`. These IDs are available to any
    //   of `alloc()`, `reserve_entity()` or `reserve_entities()`. Allocation will
    //   always prefer these over brand new IDs.
    //
    // - The `reserved` list of IDs that were once in the freelist, but got
    //   reserved by `reserve_entities` or `reserve_entity()`. They are now waiting
    //   for `flush()` to make them fully allocated.
    //
    // - The count of new IDs that do not yet exist in `self.meta()`, but which
    //   we have handed out and reserved. `flush()` will allocate room for them in `self.meta()`.
    //
    // The contents of `pending` look like this:
    //
    // ```
    // ----------------------------
    // |  freelist  |  reserved   |
    // ----------------------------
    //              ^             ^
    //          free_cursor   pending.len()
    // ```
    //
    // As IDs are allocated, `free_cursor` is atomically decremented, moving
    // items from the freelist into the reserved list by sliding over the boundary.
    //
    // Once the freelist runs out, `free_cursor` starts going negative.
    // The more negative it is, the more IDs have been reserved starting exactly at
    // the end of `meta.len()`.
    //
    // This formulation allows us to reserve any number of IDs first from the freelist
    // and then from the new IDs, using only a single atomic subtract.
    //
    // Once `flush()` is done, `free_cursor` will equal `pending.len()`.
    pending: Vec<u32>,
    free_cursor: AtomicI64,
}

impl Entities {
    /// Reserve entity IDs concurrently
    ///
    /// Storage for entity generation and location is lazily allocated by calling `flush`.
    pub fn reserve_entities(&self, count: u32) -> ReserveEntitiesIterator {
        // Use one atomic subtract to grab a range of new IDs. The range might be
        // entirely nonnegative, meaning all IDs come from the freelist, or entirely
        // negative, meaning they are all new IDs to allocate, or a mix of both.
        let range_end = self.free_cursor.fetch_sub(count as i64, Ordering::Relaxed);
        let range_start = range_end - count as i64;

        let freelist_range = range_start.max(0) as usize..range_end.max(0) as usize;

        let (new_id_start, new_id_end) = if range_start >= 0 {
            // We satisfied all requests from the freelist.
            (0, 0)
        } else {
            // We need to allocate some new Entity IDs outside of the range of self.meta.
            //
            // `range_start` covers some negative territory, e.g. `-3..6`.
            // Since the nonnegative values `0..6` are handled by the freelist, that
            // means we need to handle the negative range here.
            //
            // In this example, we truncate the end to 0, leaving us with `-3..0`.
            // Then we negate these values to indicate how far beyond the end of `meta.end()`
            // to go, yielding `meta.len()+0 .. meta.len()+3`.
            let base = self.meta.len() as i64;

            let new_id_end = u32::try_from(base - range_start).expect("too many entities");

            // `new_id_end` is in range, so no need to check `start`.
            let new_id_start = (base - range_end.min(0)) as u32;

            (new_id_start, new_id_end)
        };

        ReserveEntitiesIterator {
            meta: &self.meta[..],
            id_iter: self.pending[freelist_range].iter(),
            id_range: new_id_start..new_id_end,
        }
    }

    /// Reserve one entity ID concurrently
    ///
    /// Equivalent to `self.reserve_entities(1).next().unwrap()`, but more efficient.
    pub fn reserve_entity(&self) -> Entity {
        let n = self.free_cursor.fetch_sub(1, Ordering::Relaxed);
        if n > 0 {
            // Allocate from the freelist.
            let id = self.pending[(n - 1) as usize];
            Entity {
                generation: self.meta[id as usize].generation,
                id,
            }
        } else {
            // Grab a new ID, outside the range of `meta.len()`. `flush()` must
            // eventually be called to make it valid.
            //
            // As `self.free_cursor` goes more and more negative, we return IDs farther
            // and farther beyond `meta.len()`.
            Entity {
                generation: 0,
                id: u32::try_from(self.meta.len() as i64 - n).expect("too many entities"),
            }
        }
    }

    /// Check that we do not have pending work requiring `flush()` to be called.
    fn verify_flushed(&mut self) {
        debug_assert!(
            !self.needs_flush(),
            "flush() needs to be called before this operation is legal"
        );
    }

    /// Allocate an entity ID directly
    ///
    /// Location should be written immediately.
    pub fn alloc(&mut self) -> Entity {
        self.verify_flushed();

        if let Some(id) = self.pending.pop() {
            let new_free_cursor = self.pending.len() as i64;
            self.free_cursor.store(new_free_cursor, Ordering::Relaxed); // Not racey due to &mut self
            Entity {
                generation: self.meta[id as usize].generation,
                id,
            }
        } else {
            let id = u32::try_from(self.meta.len()).expect("too many entities");
            self.meta.push(EntityMeta::EMPTY);
            Entity { generation: 0, id }
        }
    }

    /// Destroy an entity, allowing it to be reused
    ///
    /// Must not be called while reserved entities are awaiting `flush()`.
    pub fn free(&mut self, entity: Entity) -> Result<Location, NoSuchEntity> {
        self.verify_flushed();

        let meta = &mut self.meta[entity.id as usize];
        if meta.generation != entity.generation {
            return Err(NoSuchEntity);
        }
        meta.generation += 1;

        let loc = mem::replace(&mut meta.location, EntityMeta::EMPTY.location);

        self.pending.push(entity.id);

        let new_free_cursor = self.pending.len() as i64;
        self.free_cursor.store(new_free_cursor, Ordering::Relaxed); // Not racey due to &mut self

        Ok(loc)
    }

    /// Ensure at least `n` allocations can succeed without reallocating
    pub fn reserve(&mut self, additional: u32) {
        self.verify_flushed();

        let freelist_size = self.free_cursor.load(Ordering::Relaxed);
        let shortfall = additional as i64 - freelist_size;
        if shortfall > 0 {
            self.meta.reserve(shortfall as usize);
        }
    }

    pub fn contains(&self, entity: Entity) -> bool {
        // Note that out-of-range IDs are considered to be "contained" because
        // they must be reserved IDs that we haven't flushed yet.
        self.meta
            .get(entity.id as usize)
            .map_or(true, |meta| meta.generation == entity.generation)
    }

    pub fn clear(&mut self) {
        self.meta.clear();
        self.pending.clear();
        self.free_cursor.store(0, Ordering::Relaxed); // Not racey due to &mut self
    }

    /// Access the location storage of an entity
    ///
    /// Must not be called on pending entities.
    pub fn get_mut(&mut self, entity: Entity) -> Result<&mut Location, NoSuchEntity> {
        let meta = &mut self.meta[entity.id as usize];
        if meta.generation == entity.generation {
            Ok(&mut meta.location)
        } else {
            Err(NoSuchEntity)
        }
    }

    /// Returns `Ok(Location { archetype: 0, index: undefined })` for pending entities
    pub fn get(&self, entity: Entity) -> Result<Location, NoSuchEntity> {
        if self.meta.len() <= entity.id as usize {
            return Ok(Location {
                archetype: 0,
                index: u32::max_value(),
            });
        }
        let meta = &self.meta[entity.id as usize];
        if meta.generation != entity.generation {
            return Err(NoSuchEntity);
        }
        if meta.location.archetype == 0 {
            return Ok(Location {
                archetype: 0,
                index: u32::max_value(),
            });
        }
        Ok(meta.location)
    }

    /// Panics if the given id would represent an index outside of `meta`.
    ///
    /// # Safety
    /// Must only be called for currently allocated `id`s.
    pub unsafe fn resolve_unknown_gen(&self, id: u32) -> Entity {
        let meta_len = self.meta.len();

        if meta_len > id as usize {
            let meta = &self.meta[id as usize];
            Entity {
                generation: meta.generation,
                id,
            }
        } else {
            // See if it's pending, but not yet flushed.
            let free_cursor = self.free_cursor.load(Ordering::Relaxed);
            let num_pending = cmp::max(-free_cursor, 0) as usize;

            if meta_len + num_pending > id as usize {
                // Pending entities will have generation 0.
                Entity { generation: 0, id }
            } else {
                panic!("entity id is out of range");
            }
        }
    }

    fn needs_flush(&mut self) -> bool {
        // Not racey due to &mut self
        self.free_cursor.load(Ordering::Relaxed) != self.pending.len() as i64
    }

    /// Allocates space for entities previously reserved with `reserve_entity` or
    /// `reserve_entities`, then initializes each one using the supplied function.
    pub fn flush(&mut self, mut init: impl FnMut(u32, &mut Location)) {
        // Not racey due because of self is &mut.
        let free_cursor = self.free_cursor.load(Ordering::Relaxed);

        let new_free_cursor = if free_cursor >= 0 {
            free_cursor as usize
        } else {
            let old_meta_len = self.meta.len();
            let new_meta_len = old_meta_len + -free_cursor as usize;
            self.meta.resize(new_meta_len, EntityMeta::EMPTY);

            for (id, meta) in self.meta.iter_mut().enumerate().skip(old_meta_len) {
                init(id as u32, &mut meta.location);
            }

            self.free_cursor.store(0, Ordering::Relaxed);
            0
        };

        for id in self.pending.drain(new_free_cursor..) {
            init(id, &mut self.meta[id as usize].location);
        }
    }
}

#[derive(Copy, Clone)]
pub(crate) struct EntityMeta {
    pub generation: u32,
    pub location: Location,
}

impl EntityMeta {
    const EMPTY: EntityMeta = EntityMeta {
        generation: 0,
        location: Location {
            archetype: 0,
            index: u32::max_value(), // dummy value, to be filled in
        },
    };
}

#[derive(Copy, Clone)]
pub(crate) struct Location {
    pub archetype: u32,
    pub index: u32,
}

/// Error indicating that no entity with a particular ID exists
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NoSuchEntity;

impl fmt::Display for NoSuchEntity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("no such entity")
    }
}

#[cfg(feature = "std")]
impl Error for NoSuchEntity {}

#[cfg(test)]
mod tests {
    use super::*;
    use hashbrown::{HashMap, HashSet};
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn entity_bits_roundtrip() {
        let e = Entity {
            generation: 0xDEADBEEF,
            id: 0xBAADF00D,
        };
        assert_eq!(Entity::from_bits(e.to_bits()), e);
    }

    #[test]
    fn alloc_and_free() {
        let mut rng = StdRng::seed_from_u64(0xFEEDFACEDEADF00D);

        let mut e = Entities::default();
        let mut first_unused = 0u32;
        let mut id_to_gen: HashMap<u32, u32> = Default::default();
        let mut free_set: HashSet<u32> = Default::default();

        for _ in 0..100 {
            let alloc = rng.gen_bool(0.7);
            if alloc || first_unused == 0 {
                let entity = e.alloc();

                let id = entity.id;
                if !free_set.is_empty() {
                    // This should have come from the freelist.
                    assert!(free_set.remove(&id));
                } else if id >= first_unused {
                    first_unused = id + 1;
                }

                e.get_mut(entity).unwrap().index = 37;

                assert!(id_to_gen.insert(id, entity.generation).is_none());
            } else {
                // Free a random ID, whether or not it's in use, and check for errors.
                let id = rng.gen_range(0, first_unused);

                let generation = id_to_gen.remove(&id);
                let entity = Entity {
                    id,
                    generation: generation.unwrap_or(0),
                };

                assert_eq!(e.free(entity).is_ok(), generation.is_some());

                free_set.insert(id);
            }
        }
    }

    #[test]
    fn contains() {
        let mut e = Entities::default();

        for _ in 0..2 {
            let entity = e.alloc();
            assert!(e.contains(entity));

            e.free(entity).unwrap();
            assert!(!e.contains(entity));
        }

        // Reserved but not flushed are still "contained".
        for _ in 0..3 {
            let entity = e.reserve_entity();
            assert!(e.contains(entity));
        }
    }

    // Shared test code parameterized by how we want to allocate an Entity block.
    fn reserve_test_helper(reserve_n: impl FnOnce(&mut Entities, u32) -> Vec<Entity>) {
        let mut e = Entities::default();

        // Allocate 10 items.
        let mut v1: Vec<Entity> = (0..10).map(|_| e.alloc()).collect();
        assert_eq!(v1.iter().map(|e| e.id).max(), Some(9));
        for &entity in v1.iter() {
            assert!(e.contains(entity));
            e.get_mut(entity).unwrap().index = 37;
        }

        // Put the last 4 on the freelist.
        for entity in v1.drain(6..) {
            e.free(entity).unwrap();
        }
        assert_eq!(e.free_cursor.load(Ordering::Relaxed), 4);

        // Reserve 10 entities, so 4 will come from the freelist.
        // This means we will have allocated 10 + 10 - 4 total items, so max id is 15.
        let v2 = reserve_n(&mut e, 10);
        assert_eq!(v2.iter().map(|e| e.id).max(), Some(15));

        // Reserved IDs still count as "contained".
        assert!(v2.iter().all(|&entity| e.contains(entity)));

        // We should have exactly IDs 0..16
        let mut v3: Vec<Entity> = v1.iter().chain(v2.iter()).copied().collect();
        assert_eq!(v3.len(), 16);
        v3.sort_by_key(|entity| entity.id);
        for (i, entity) in v3.into_iter().enumerate() {
            assert_eq!(entity.id, i as u32);
        }

        // 6 will come from pending.
        assert_eq!(e.free_cursor.load(Ordering::Relaxed), -6);

        let mut flushed = Vec::new();
        e.flush(|id, _| flushed.push(id));
        flushed.sort_unstable();

        assert_eq!(flushed, (6..16).collect::<Vec<_>>());
    }

    #[test]
    fn reserve_entity() {
        reserve_test_helper(|e, n| (0..n).map(|_| e.reserve_entity()).collect())
    }

    #[test]
    fn reserve_entities() {
        reserve_test_helper(|e, n| e.reserve_entities(n).collect())
    }
}