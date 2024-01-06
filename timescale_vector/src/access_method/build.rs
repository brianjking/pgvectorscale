use std::time::Instant;

use pgrx::*;

use crate::access_method::builder_graph::WriteStats;
use crate::access_method::disk_index_graph::DiskIndexGraph;
use crate::access_method::graph;
use crate::access_method::graph::Graph;
use crate::access_method::graph::GraphNeighborStore;
use crate::access_method::graph::InsertStats;
use crate::access_method::model::PgVector;
use crate::access_method::options::TSVIndexOptions;

use crate::util::page::PageType;
use crate::util::tape::Tape;
use crate::util::*;

use super::builder_graph::BuilderGraph;

use super::meta_page::MetaPage;

use super::storage;
use super::storage::{Storage, StorageTrait};

struct OuterBuildState<'a, 'b, 'c> {
    inner: BuildState<'a, 'b>,
    storage: Storage<'c>,
}

impl<'a, 'b, 'c> OuterBuildState<'a, 'b, 'c> {
    fn new(
        index_relation: &'a PgRelation,
        meta_page: MetaPage,
        graph: Graph<'b>,
        mut storage: Storage<'c>,
    ) -> Self {
        let page_type = match &mut storage {
            Storage::None => {
                pgrx::error!("not implemented");
            }
            Storage::PQ(pq) => {
                pq.start_training(&meta_page);
                pgrx::error!("not implemented");
            }
            Storage::BQ(bq) => {
                bq.start_training(&meta_page);
                bq.page_type()
            }
        };
        OuterBuildState {
            inner: BuildState::new(index_relation, meta_page, graph, page_type),
            storage,
        }
    }
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
    let vector = (*vec).to_slice();
    let heap_pointer = ItemPointer::with_item_pointer_data(*heap_tid);
    let mut meta_page = MetaPage::read(&index_relation);

    let mut storage =
        meta_page.get_storage(Some(&heap_relation), Some(get_attribute_number(index_info)));
    match &mut storage {
        Storage::None => {}
        Storage::PQ(pq) => {
            pq.load(&index_relation, &meta_page);
            //let _stats = insert_storage(&pq, &index_relation, vector, heap_pointer, &mut meta_page);
            pgrx::error!("not implemented");
        }
        Storage::BQ(bq) => {
            bq.load(&index_relation, &meta_page);
            let _stats = insert_storage(bq, &index_relation, vector, heap_pointer, &mut meta_page);
        }
    }
    false
}

unsafe fn insert_storage<S: StorageTrait>(
    storage: &S,
    index_relation: &PgRelation,
    vector: &[f32],
    heap_pointer: ItemPointer,
    meta_page: &mut MetaPage,
) -> InsertStats {
    let mut tape = Tape::new(&index_relation, storage.page_type());
    let index_pointer = storage.create_node(
        &&index_relation,
        vector,
        heap_pointer,
        &meta_page,
        &mut tape,
    );

    let mut graph = Graph::new(GraphNeighborStore::Disk(DiskIndexGraph::new()), meta_page);
    graph.insert(&index_relation, index_pointer, vector, storage)
}

#[pg_guard]
pub extern "C" fn ambuildempty(_index_relation: pg_sys::Relation) {
    panic!("ambuildempty: not yet implemented")
}

fn get_attribute_number(index_info: *mut pg_sys::IndexInfo) -> pg_sys::AttrNumber {
    unsafe { assert!((*index_info).ii_NumIndexAttrs == 1) };
    unsafe { (*index_info).ii_IndexAttrNumbers[0] }
}

fn do_heap_scan<'a>(
    index_info: *mut pg_sys::IndexInfo,
    heap_relation: &'a PgRelation,
    index_relation: &'a PgRelation,
    mut meta_page: MetaPage,
) -> usize {
    let storage =
        meta_page.get_storage(Some(heap_relation), Some(get_attribute_number(index_info)));

    let mut state = OuterBuildState::new(
        index_relation,
        meta_page.clone(),
        Graph::new(
            GraphNeighborStore::Builder(BuilderGraph::new()),
            &mut meta_page,
        ),
        storage,
    );
    unsafe {
        pg_sys::IndexBuildHeapScan(
            heap_relation.as_ptr(),
            index_relation.as_ptr(),
            index_info,
            Some(build_callback),
            &mut state,
        );
    }

    // we train the quantizer and add prepare to write quantized values to the nodes.
    let write_stats = match &mut state.storage {
        Storage::None => {
            error!("not implemented");
        }
        Storage::PQ(pq) => {
            pq.finish_training();
            error!("not implemented")
        }
        Storage::BQ(bq) => bq.finish_training(index_relation, &state.inner.graph),
    };

    info!("write done");
    assert_eq!(write_stats.num_nodes, state.inner.ntuples);

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
    let ntuples = state.inner.ntuples;

    warning!("Indexed {} tuples", ntuples);

    match state.storage {
        Storage::None => {}
        Storage::PQ(pq) => {
            pq.write_metadata(index_relation);
        }
        Storage::BQ(bq) => {
            bq.write_metadata(index_relation);
        }
    }

    ntuples
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
        let state = (state as *mut OuterBuildState).as_mut().unwrap();

        let mut old_context = state.inner.memcxt.set_as_current();
        let heap_pointer = ItemPointer::with_item_pointer_data(*ctid);

        match &mut state.storage {
            Storage::None => {
                pgrx::error!("not implemented");
            }
            Storage::PQ(pq) => {
                pgrx::error!("not implemented");
            }
            Storage::BQ(bq) => {
                build_callback_internal(
                    index_relation,
                    heap_pointer,
                    (*vec).to_slice(),
                    &mut state.inner,
                    bq,
                );
            }
        }

        old_context.set_as_current();
        state.inner.memcxt.reset();
    }
    //todo: what do we do with nulls?
}

#[inline(always)]
fn build_callback_internal<S: StorageTrait>(
    index: PgRelation,
    heap_pointer: ItemPointer,
    vector: &[f32],
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
            state.stats.greedy_search_stats.distance_comparisons / state.ntuples,
            state.stats,
        );
    }

    storage.add_sample(vector);

    let index_pointer = storage.create_node(
        &index,
        vector,
        heap_pointer,
        &state.meta_page,
        &mut state.tape,
    );

    let new_stats = state.graph.insert(&index, index_pointer, vector, storage);
    state.stats.combine(new_stats);
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::*;

    #[pg_test]
    unsafe fn test_index_creation() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors=30);

            set enable_seqscan =0;
            select * from test order by embedding <=> '[0,0,0]';
            explain analyze select * from test order by embedding <=> '[0,0,0]';
            drop index idxtest;
            ",
        ))?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_pq_index_creation() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test_pq (
                embedding vector (1536)
            );

           -- generate 300 vectors
            INSERT INTO test_pq (embedding)
            SELECT
                *
            FROM (
                SELECT
                    ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
                FROM
                    generate_series(1, 1536 * 300) i
                GROUP BY
                    i % 300) g;

            CREATE INDEX idx_tsv_pq ON test_pq USING tsv (embedding) WITH (num_neighbors = 64, search_list_size = 125, max_alpha = 1.0, use_pq = TRUE, pq_vector_length = 64);

            ;

            SET enable_seqscan = 0;
            -- perform index scans on the vectors
            SELECT
                *
            FROM
                test_pq
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            EXPLAIN ANALYZE
            SELECT
                *
            FROM
                test_pq
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            DROP INDEX idx_tsv_pq;
            ",
        ))?;
        Ok(())
    }

    unsafe fn test_bq_index_creation_params(num_neighbors: usize) -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test_bq (
                embedding vector (1536)
            );

           -- generate 300 vectors
            INSERT INTO test_bq (embedding)
            SELECT
                *
            FROM (
                SELECT
                    ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
                FROM
                    generate_series(1, 1536 * 300) i
                GROUP BY
                    i % 300) g;

            CREATE INDEX idx_tsv_bq ON test_bq USING tsv (embedding) WITH (num_neighbors = {num_neighbors}, search_list_size = 125, max_alpha = 1.0, use_bq = TRUE);

            ;

            SET enable_seqscan = 0;
            -- perform index scans on the vectors
            SELECT
                *
            FROM
                test_bq
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            -- test insert 2 vectors
            INSERT INTO test_bq (embedding)
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
                test_bq
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            DROP INDEX idx_tsv_bq;
            ",
        ))?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_bq_index_creation() -> spi::Result<()> {
        test_bq_index_creation_params(38)?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_bq_index_creation_few_neighbors() -> spi::Result<()> {
        //a test with few neighbors tests the case that nodes share a page, which has caused deadlocks in the past.
        test_bq_index_creation_params(10)?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_insert() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors=30);

            INSERT INTO test(embedding) VALUES ('[11,12,13]');
            ",
        ))?;

        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(4, res.unwrap());

        Spi::run(&format!(
            "INSERT INTO test(embedding) VALUES ('[11,12,13]'),  ('[14,15,16]');",
        ))?;
        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(6, res.unwrap());

        Spi::run(&format!("drop index idxtest;",))?;

        Ok(())
    }

    #[pg_test]
    unsafe fn test_empty_table_insert() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors=30);

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');
            ",
        ))?;

        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(3, res.unwrap());

        Spi::run(&format!("drop index idxtest;",))?;

        Ok(())
    }

    #[pg_test]
    unsafe fn test_insert_empty_insert() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors=30);

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
