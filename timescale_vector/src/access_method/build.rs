use std::time::Instant;

use pgrx::*;

use crate::access_method::graph::Graph;
use crate::access_method::graph_neighbor_store::GraphNeighborStore;
use crate::access_method::options::TSVIndexOptions;
use crate::access_method::pg_vector::PgVector;
use crate::access_method::stats::{InsertStats, WriteStats};

use crate::util::page::PageType;
use crate::util::tape::Tape;
use crate::util::*;

use super::bq::BqSpeedupStorage;
use super::graph_neighbor_store::BuilderNeighborCache;

use super::meta_page::MetaPage;

use super::plain_storage::PlainStorage;
use super::pq_storage::PqCompressionStorage;
use super::storage::{Storage, StorageType};

enum StorageBuildState<'a, 'b, 'c, 'd, 'e> {
    BqSpeedup(&'a mut BqSpeedupStorage<'b>, &'c mut BuildState<'d, 'e>),
    PqCompression(&'a mut PqCompressionStorage<'b>, &'c mut BuildState<'d, 'e>),
    Plain(&'a mut PlainStorage<'b>, &'c mut BuildState<'d, 'e>),
}

struct BuildState<'a, 'b> {
    memcxt: PgMemoryContexts,
    meta_page: MetaPage,
    ntuples: usize,
    tape: Tape<'a>, //The tape is a memory abstraction over Postgres pages for writing data.
    graph: Graph<'b>,
    started: Instant,
    stats: InsertStats,
}

impl<'a, 'b> BuildState<'a, 'b> {
    fn new(
        index_relation: &'a PgRelation,
        meta_page: MetaPage,
        graph: Graph<'b>,
        page_type: PageType,
    ) -> Self {
        let tape = unsafe { Tape::new(index_relation, page_type) };

        //TODO: some ways to get rid of meta_page.clone?
        BuildState {
            memcxt: PgMemoryContexts::new("tsv build context"),
            ntuples: 0,
            meta_page: meta_page,
            tape,
            graph: graph,
            started: Instant::now(),
            stats: InsertStats::new(),
        }
    }
}

#[pg_guard]
pub extern "C" fn ambuild(
    heaprel: pg_sys::Relation,
    indexrel: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    let heap_relation = unsafe { PgRelation::from_pg(heaprel) };
    let index_relation = unsafe { PgRelation::from_pg(indexrel) };
    let opt = TSVIndexOptions::from_relation(&index_relation);

    notice!(
        "Starting index build. num_neighbors={} search_list_size={}, max_alpha={}, use_pq={}, pq_vector_length={}",
        opt.num_neighbors,
        opt.search_list_size,
        opt.max_alpha,
        opt.use_pq,
        opt.pq_vector_length
    );

    let dimensions = index_relation.tuple_desc().get(0).unwrap().atttypmod;
    // PQ is only applicable to high dimension vectors.
    if opt.use_pq {
        if dimensions < opt.pq_vector_length as i32 {
            error!("use_pq can only be applied to vectors with greater than {} dimensions. {} dimensions provided", opt.pq_vector_length, dimensions)
        };
        if dimensions % opt.pq_vector_length as i32 != 0 {
            error!("use_pq can only be applied to vectors where the number of dimensions {} is divisible by the pq_vector_length {} ", dimensions, opt.pq_vector_length)
        };
    }
    assert!(dimensions > 0 && dimensions < 2000);
    let meta_page = unsafe { MetaPage::create(&index_relation, dimensions as _, opt.clone()) };

    let ntuples = do_heap_scan(index_info, &heap_relation, &index_relation, meta_page);

    let mut result = unsafe { PgBox::<pg_sys::IndexBuildResult>::alloc0() };
    result.heap_tuples = ntuples as f64;
    result.index_tuples = ntuples as f64;

    result.into_pg()
}

#[pg_guard]
pub unsafe extern "C" fn aminsert(
    indexrel: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    heaprel: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck,
    _index_unchanged: bool,
    index_info: *mut pg_sys::IndexInfo,
) -> bool {
    let index_relation = unsafe { PgRelation::from_pg(indexrel) };
    let heap_relation = unsafe { PgRelation::from_pg(heaprel) };
    let vec = PgVector::from_pg_parts(values, isnull, 0);
    if let None = vec {
        //todo handle NULLs?
        return false;
    }
    let vec = vec.unwrap();
    let heap_pointer = ItemPointer::with_item_pointer_data(*heap_tid);
    let mut meta_page = MetaPage::fetch(&index_relation);

    let mut storage = meta_page.get_storage_type();
    let mut stats = InsertStats::new();
    match &mut storage {
        StorageType::Plain => {
            let plain = PlainStorage::load_for_insert(&index_relation);
            insert_storage(
                &plain,
                &index_relation,
                vec,
                heap_pointer,
                &mut meta_page,
                &mut stats,
            );
        }
        StorageType::PqCompression => {
            let pq = PqCompressionStorage::load_for_insert(
                &heap_relation,
                get_attribute_number(index_info),
                &index_relation,
                &meta_page,
                &mut stats.quantizer_stats,
            );
            insert_storage(
                &pq,
                &index_relation,
                vec,
                heap_pointer,
                &mut meta_page,
                &mut stats,
            );
        }
        StorageType::BqSpeedup => {
            let bq = BqSpeedupStorage::load_for_insert(
                &heap_relation,
                get_attribute_number(index_info),
                &index_relation,
                &meta_page,
                &mut stats.quantizer_stats,
            );
            insert_storage(
                &bq,
                &index_relation,
                vec,
                heap_pointer,
                &mut meta_page,
                &mut stats,
            );
        }
    }
    false
}

unsafe fn insert_storage<S: Storage>(
    storage: &S,
    index_relation: &PgRelation,
    vector: PgVector,
    heap_pointer: ItemPointer,
    meta_page: &mut MetaPage,
    stats: &mut InsertStats,
) {
    let mut tape = Tape::new(&index_relation, S::page_type());
    let index_pointer = storage.create_node(
        vector.to_slice(),
        heap_pointer,
        &meta_page,
        &mut tape,
        stats,
    );

    let mut graph = Graph::new(GraphNeighborStore::Disk, meta_page);
    graph.insert(&index_relation, index_pointer, vector, storage, stats)
}

#[pg_guard]
pub extern "C" fn ambuildempty(_index_relation: pg_sys::Relation) {
    panic!("ambuildempty: not yet implemented")
}

pub fn get_attribute_number(index_info: *mut pg_sys::IndexInfo) -> pg_sys::AttrNumber {
    unsafe { assert!((*index_info).ii_NumIndexAttrs == 1) };
    unsafe { (*index_info).ii_IndexAttrNumbers[0] }
}

fn do_heap_scan<'a>(
    index_info: *mut pg_sys::IndexInfo,
    heap_relation: &'a PgRelation,
    index_relation: &'a PgRelation,
    meta_page: MetaPage,
) -> usize {
    let storage = meta_page.get_storage_type();

    let mut mp2 = meta_page.clone();
    let graph = Graph::new(
        GraphNeighborStore::Builder(BuilderNeighborCache::new()),
        &mut mp2,
    );
    let mut write_stats = WriteStats::new();
    match storage {
        StorageType::Plain => {
            let mut plain = PlainStorage::new_for_build(index_relation);
            plain.start_training(&meta_page);
            let page_type = PlainStorage::page_type();
            let mut bs = BuildState::new(index_relation, meta_page, graph, page_type);
            let mut state = StorageBuildState::Plain(&mut plain, &mut bs);

            unsafe {
                pg_sys::IndexBuildHeapScan(
                    heap_relation.as_ptr(),
                    index_relation.as_ptr(),
                    index_info,
                    Some(build_callback),
                    &mut state,
                );
            }

            finalize_index_build(&mut plain, &mut bs, write_stats)
        }
        StorageType::PqCompression => {
            let mut pq = PqCompressionStorage::new_for_build(
                index_relation,
                heap_relation,
                get_attribute_number(index_info),
            );
            pq.start_training(&meta_page);
            unsafe {
                pg_sys::IndexBuildHeapScan(
                    heap_relation.as_ptr(),
                    index_relation.as_ptr(),
                    index_info,
                    Some(build_callback_pq_train),
                    &mut pq,
                );
            }
            pq.finish_training(&mut write_stats);

            let page_type = PqCompressionStorage::page_type();
            let mut bs = BuildState::new(index_relation, meta_page, graph, page_type);
            let mut state = StorageBuildState::PqCompression(&mut pq, &mut bs);

            unsafe {
                pg_sys::IndexBuildHeapScan(
                    heap_relation.as_ptr(),
                    index_relation.as_ptr(),
                    index_info,
                    Some(build_callback),
                    &mut state,
                );
            }

            finalize_index_build(&mut pq, &mut bs, write_stats)
        }
        StorageType::BqSpeedup => {
            let mut bq = BqSpeedupStorage::new_for_build(
                index_relation,
                heap_relation,
                get_attribute_number(index_info),
            );
            bq.start_training(&meta_page);
            unsafe {
                pg_sys::IndexBuildHeapScan(
                    heap_relation.as_ptr(),
                    index_relation.as_ptr(),
                    index_info,
                    Some(build_callback_bq_train),
                    &mut bq,
                );
            }
            bq.finish_training(&mut write_stats);

            let page_type = BqSpeedupStorage::page_type();
            let mut bs = BuildState::new(index_relation, meta_page, graph, page_type);
            let mut state = StorageBuildState::BqSpeedup(&mut bq, &mut bs);

            unsafe {
                pg_sys::IndexBuildHeapScan(
                    heap_relation.as_ptr(),
                    index_relation.as_ptr(),
                    index_info,
                    Some(build_callback),
                    &mut state,
                );
            }

            finalize_index_build(&mut bq, &mut bs, write_stats)
        }
    }
}

fn finalize_index_build<S: Storage>(
    storage: &mut S,
    state: &mut BuildState,
    mut write_stats: WriteStats,
) -> usize {
    match state.graph.get_neighbor_store() {
        GraphNeighborStore::Builder(builder) => {
            for (&index_pointer, neighbors) in builder.iter() {
                write_stats.num_nodes += 1;
                let prune_neighbors;
                let neighbors =
                    if neighbors.len() > state.graph.get_meta_page().get_num_neighbors() as _ {
                        //OPT: get rid of this clone
                        prune_neighbors = state.graph.prune_neighbors(
                            neighbors.clone(),
                            storage,
                            &mut write_stats.prune_stats,
                        );
                        &prune_neighbors
                    } else {
                        neighbors
                    };
                write_stats.num_neighbors += neighbors.len();

                storage.finalize_node_at_end_of_build(
                    &state.meta_page,
                    index_pointer,
                    neighbors,
                    &mut write_stats,
                );
            }
        }
        GraphNeighborStore::Disk => {
            panic!("Should not be using the disk neighbor store during build");
        }
    }

    info!("write done");
    assert_eq!(write_stats.num_nodes, state.ntuples);

    let writing_took = Instant::now()
        .duration_since(write_stats.started)
        .as_secs_f64();
    if write_stats.num_nodes > 0 {
        info!(
            "Writing took {}s or {}s/tuple.  Avg neighbors: {}",
            writing_took,
            writing_took / write_stats.num_nodes as f64,
            write_stats.num_neighbors / write_stats.num_nodes
        );
    }
    if write_stats.prune_stats.calls > 0 {
        info!(
            "When pruned for cleanup: avg neighbors before/after {}/{} of {} prunes",
            write_stats.prune_stats.num_neighbors_before_prune / write_stats.prune_stats.calls,
            write_stats.prune_stats.num_neighbors_after_prune / write_stats.prune_stats.calls,
            write_stats.prune_stats.calls
        );
    }
    let ntuples = state.ntuples;

    warning!("Indexed {} tuples", ntuples);

    ntuples
}

#[pg_guard]
unsafe extern "C" fn build_callback_bq_train(
    _index: pg_sys::Relation,
    _ctid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut std::os::raw::c_void,
) {
    let vec = PgVector::from_pg_parts(values, isnull, 0);
    if let Some(vec) = vec {
        let bq = (state as *mut BqSpeedupStorage).as_mut().unwrap();
        bq.add_sample(vec.to_slice());
    }
}

#[pg_guard]
unsafe extern "C" fn build_callback_pq_train(
    _index: pg_sys::Relation,
    _ctid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut std::os::raw::c_void,
) {
    let vec = PgVector::from_pg_parts(values, isnull, 0);
    if let Some(vec) = vec {
        let pq = (state as *mut PqCompressionStorage).as_mut().unwrap();
        pq.add_sample(vec.to_slice());
    }
}

#[pg_guard]
unsafe extern "C" fn build_callback(
    index: pg_sys::Relation,
    ctid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut std::os::raw::c_void,
) {
    let index_relation = unsafe { PgRelation::from_pg(index) };
    let vec = PgVector::from_pg_parts(values, isnull, 0);
    if let Some(vec) = vec {
        let state = (state as *mut StorageBuildState).as_mut().unwrap();
        let heap_pointer = ItemPointer::with_item_pointer_data(*ctid);

        match state {
            StorageBuildState::BqSpeedup(bq, state) => {
                build_callback_memory_wrapper(index_relation, heap_pointer, vec, state, *bq);
            }
            StorageBuildState::PqCompression(pq, state) => {
                build_callback_memory_wrapper(index_relation, heap_pointer, vec, state, *pq);
            }
            StorageBuildState::Plain(plain, state) => {
                build_callback_memory_wrapper(index_relation, heap_pointer, vec, state, *plain);
            }
        }
    }
}

#[inline(always)]
unsafe fn build_callback_memory_wrapper<S: Storage>(
    index: PgRelation,
    heap_pointer: ItemPointer,
    vector: PgVector,
    state: &mut BuildState,
    storage: &mut S,
) {
    let mut old_context = state.memcxt.set_as_current();

    build_callback_internal(index, heap_pointer, vector, state, storage);

    old_context.set_as_current();
    state.memcxt.reset();
}

#[inline(always)]
fn build_callback_internal<S: Storage>(
    index: PgRelation,
    heap_pointer: ItemPointer,
    vector: PgVector,
    state: &mut BuildState,
    storage: &mut S,
) {
    check_for_interrupts!();

    state.ntuples = state.ntuples + 1;

    if state.ntuples % 1000 == 0 {
        info!(
            "Processed {} tuples in {}s which is {}s/tuple. Dist/tuple: Prune: {} search: {}. Stats: {:?}",
            state.ntuples,
            Instant::now().duration_since(state.started).as_secs_f64(),
            (Instant::now().duration_since(state.started) / state.ntuples as u32).as_secs_f64(),
            state.stats.prune_neighbor_stats.distance_comparisons / state.ntuples,
            state.stats.greedy_search_stats.get_total_distance_comparisons() / state.ntuples,
            state.stats,
        );
    }

    let index_pointer = storage.create_node(
        vector.to_slice(),
        heap_pointer,
        &state.meta_page,
        &mut state.tape,
        &mut state.stats,
    );

    state
        .graph
        .insert(&index, index_pointer, vector, storage, &mut state.stats);
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
pub mod tests {
    use std::collections::HashSet;

    use pgrx::*;

    use crate::util::ItemPointer;

    //TODO: add test where inserting and querying with vectors that are all the same.

    #[cfg(any(test, feature = "pg_test"))]
    pub unsafe fn test_index_creation_and_accuracy_scaffold(
        index_options: &str,
    ) -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test_data (
                embedding vector (1536)
            );

            select setseed(0.5);
           -- generate 300 vectors
            INSERT INTO test_data (embedding)
            SELECT
                *
            FROM (
                SELECT
                    ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
                FROM
                    generate_series(1, 1536 * 300) i
                GROUP BY
                    i % 300) g;

            CREATE INDEX idx_tsv_bq ON test_data USING tsv (embedding) WITH ({index_options});


            SET enable_seqscan = 0;
            -- perform index scans on the vectors
            SELECT
                *
            FROM
                test_data
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));"))?;

        let test_vec: Option<Vec<f32>> = Spi::get_one(&format!(
            "SELECT('{{' || array_to_string(array_agg(1.0), ',', '0') || '}}')::real[] AS embedding
    FROM generate_series(1, 1536)"
        ))?;

        let cnt: Option<i64> = Spi::get_one_with_args(
                &format!(
                    "
            SET enable_seqscan = 0;
            SET enable_indexscan = 1;
            SET tsv.query_search_list_size = 2;
            WITH cte as (select * from test_data order by embedding <=> $1::vector) SELECT count(*) from cte;
            ",
                ),
                vec![(
                    pgrx::PgOid::Custom(pgrx::pg_sys::FLOAT4ARRAYOID),
                    test_vec.clone().into_datum(),
                )],
            )?;

        //FIXME: should work in all cases
        if !index_options.contains("num_neighbors=10") {
            assert_eq!(cnt.unwrap(), 300, "initial count");
        }

        Spi::run(&format!("
            -- test insert 2 vectors
            INSERT INTO test_data (embedding)
            SELECT
                *
            FROM (
                SELECT
                    ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
                FROM
                    generate_series(1, 1536 * 2) i
                GROUP BY
                    i % 2) g;


            EXPLAIN ANALYZE
            SELECT
                *
            FROM
                test_data
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            -- test insert 10 vectors to search for that aren't random
            INSERT INTO test_data (embedding)
            SELECT
                *
            FROM (
                SELECT
                    ('[' || array_to_string(array_agg(1.0), ',', '0') || ']')::vector AS embedding
                FROM
                    generate_series(1, 1536 * 10) i
                GROUP BY
                    i % 10) g;

            ",
        ))?;

        let with_index: Option<Vec<pgrx::pg_sys::ItemPointerData>> = Spi::get_one_with_args(
            &format!(
                "
        SET enable_seqscan = 0;
        SET enable_indexscan = 1;
        SET tsv.query_search_list_size = 25;
        WITH cte AS (
            SELECT
                ctid
            FROM
                test_data
            ORDER BY
                embedding <=> $1::vector
            LIMIT 10
        )
        SELECT array_agg(ctid) from cte;"
            ),
            vec![(
                pgrx::PgOid::Custom(pgrx::pg_sys::FLOAT4ARRAYOID),
                test_vec.clone().into_datum(),
            )],
        )?;

        /* Test that the explain plan is generated ok */
        let explain: Option<pgrx::datum::Json> = Spi::get_one_with_args(
            &format!(
                "
        SET enable_seqscan = 0;
        SET enable_indexscan = 1;
        EXPLAIN (format json) WITH cte AS (
            SELECT
                ctid
            FROM
                test_data
            ORDER BY
                embedding <=> $1::vector
            LIMIT 10
        )
        SELECT array_agg(ctid) from cte;"
            ),
            vec![(
                pgrx::PgOid::Custom(pgrx::pg_sys::FLOAT4ARRAYOID),
                test_vec.clone().into_datum(),
            )],
        )?;
        assert!(explain.is_some());
        //warning!("explain: {}", explain.unwrap().0);

        let without_index: Option<Vec<pgrx::pg_sys::ItemPointerData>> = Spi::get_one_with_args(
            &format!(
                "
        SET enable_seqscan = 1;
        SET enable_indexscan = 0;
        WITH cte AS (
            SELECT
                ctid
            FROM
                test_data
            ORDER BY
                embedding <=> $1::vector
            LIMIT 10
        )
        SELECT array_agg(ctid) from cte;"
            ),
            vec![(
                pgrx::PgOid::Custom(pgrx::pg_sys::FLOAT4ARRAYOID),
                test_vec.clone().into_datum(),
            )],
        )?;

        let set: HashSet<_> = without_index
            .unwrap()
            .iter()
            .map(|&ctid| ItemPointer::with_item_pointer_data(ctid))
            .collect();

        let mut matches = 0;
        for ctid in with_index.unwrap() {
            if set.contains(&ItemPointer::with_item_pointer_data(ctid)) {
                matches += 1;
            }
        }
        assert!(matches > 9, "Low number of matches: {}", matches);

        //FIXME: should work in all cases
        if !index_options.contains("num_neighbors=10") {
            //make sure you can scan entire table with index
            let cnt: Option<i64> = Spi::get_one_with_args(
            &format!(
                "
        SET enable_seqscan = 0;
        SET enable_indexscan = 1;
        SET tsv.query_search_list_size = 2;
        WITH cte as (select * from test_data order by embedding <=> $1::vector) SELECT count(*) from cte;
        ",
            ),
            vec![(
                pgrx::PgOid::Custom(pgrx::pg_sys::FLOAT4ARRAYOID),
                test_vec.into_datum(),
            )],
        )?;

            assert_eq!(cnt.unwrap(), 312);
        }

        Ok(())
    }

    #[cfg(any(test, feature = "pg_test"))]
    pub unsafe fn test_empty_table_insert_scaffold(index_options: &str) -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH ({index_options});

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');
            ",
        ))?;

        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(3, res.unwrap());

        Spi::run(&format!(
            "
        set enable_seqscan = 0;
        explain analyze select * from test order by embedding <=> '[0,0,0]';
        ",
        ))?;

        Spi::run(&format!("drop index idxtest;",))?;

        Ok(())
    }

    #[cfg(any(test, feature = "pg_test"))]
    pub unsafe fn test_insert_empty_insert_scaffold(index_options: &str) -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH ({index_options});

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');
            DELETE FROM test;
            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[14,15,16]');
            ",
        ))?;

        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(2, res.unwrap());

        Spi::run(&format!("drop index idxtest;",))?;

        Ok(())
    }
}
