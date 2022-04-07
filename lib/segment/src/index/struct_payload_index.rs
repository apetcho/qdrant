use std::collections::{HashMap, HashSet};
use std::fs::{create_dir_all, remove_file, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomic_refcell::AtomicRefCell;
use itertools::Itertools;
use log::debug;
use serde_json::Value;

use crate::entry::entry_point::{OperationError, OperationResult};
use crate::id_tracker::IdTrackerSS;
use crate::index::field_index::index_selector::index_selector;
use crate::index::field_index::{CardinalityEstimation, PayloadBlockCondition, PrimaryCondition};
use crate::index::field_index::{FieldIndex, PayloadFieldIndex};
use crate::index::payload_config::PayloadConfig;
use crate::index::query_estimator::estimate_filter;
use crate::index::visited_pool::VisitedPool;
use crate::index::PayloadIndex;
use crate::payload_storage::condition_checker::ValueChecker;
use crate::payload_storage::query_checker::check_filter;
use crate::payload_storage::{ConditionCheckerSS, FilterContext, PayloadStorageSS};
use crate::types::{
    Condition, FieldCondition, Filter, GeoPoint, IsEmptyCondition, PayloadKeyType,
    PayloadKeyTypeRef, PayloadSchemaType, PointOffsetType,
};
use crate::vector_storage::VectorStorageSS;

pub const PAYLOAD_FIELD_INDEX_PATH: &str = "fields";

type IndexesMap = HashMap<PayloadKeyType, Vec<FieldIndex>>;

/// `PayloadIndex` implementation, which actually uses index structures for providing faster search
pub struct StructPayloadIndex {
    condition_checker: Arc<ConditionCheckerSS>,
    vector_storage: Arc<AtomicRefCell<VectorStorageSS>>,
    /// Payload storage
    payload: Arc<AtomicRefCell<PayloadStorageSS>>,
    id_tracker: Arc<AtomicRefCell<IdTrackerSS>>,
    /// Indexes, associated with fields
    field_indexes: IndexesMap,
    config: PayloadConfig,
    /// Root of index persistence dir
    path: PathBuf,
    visited_pool: VisitedPool,
}

impl StructPayloadIndex {
    pub fn estimate_field_condition(
        &self,
        condition: &FieldCondition,
    ) -> Option<CardinalityEstimation> {
        self.field_indexes.get(&condition.key).and_then(|indexes| {
            let mut result_estimation: Option<CardinalityEstimation> = None;
            for index in indexes {
                result_estimation = index.estimate_cardinality(condition);
                if result_estimation.is_some() {
                    break;
                }
            }
            result_estimation
        })
    }

    fn query_field(
        &self,
        field_condition: &FieldCondition,
    ) -> Option<Box<dyn Iterator<Item = PointOffsetType> + '_>> {
        let indexes = self
            .field_indexes
            .get(&field_condition.key)
            .and_then(|indexes| {
                indexes
                    .iter()
                    .map(|field_index| field_index.filter(field_condition))
                    .find(|filter_iter| filter_iter.is_some())
                    .map(|filter_iter| filter_iter.unwrap())
            });
        indexes
    }

    fn config_path(&self) -> PathBuf {
        PayloadConfig::get_config_path(&self.path)
    }

    fn save_config(&self) -> OperationResult<()> {
        let config_path = self.config_path();
        self.config.save(&config_path)
    }

    fn get_field_index_dir(path: &Path) -> PathBuf {
        path.join(PAYLOAD_FIELD_INDEX_PATH)
    }

    fn get_field_index_path(path: &Path, field: PayloadKeyTypeRef) -> PathBuf {
        Self::get_field_index_dir(path).join(format!("{}.idx", field))
    }

    fn save_field_index(&self, field: PayloadKeyTypeRef) -> OperationResult<()> {
        let field_index_dir = Self::get_field_index_dir(&self.path);
        let field_index_path = Self::get_field_index_path(&self.path, field);
        create_dir_all(field_index_dir)?;

        match self.field_indexes.get(field) {
            None => {}
            Some(indexes) => {
                let file = File::create(&field_index_path)?;
                serde_cbor::to_writer(file, indexes).map_err(|err| {
                    OperationError::service_error(&format!("Unable to save index: {:?}", err))
                })?;
            }
        }
        Ok(())
    }

    fn load_or_build_field_index(
        &self,
        field: PayloadKeyTypeRef,
        payload_type: PayloadSchemaType,
    ) -> OperationResult<Vec<FieldIndex>> {
        let field_index_path = Self::get_field_index_path(&self.path, field);
        if field_index_path.exists() {
            debug!(
                "Loading field `{}` index from {}",
                field,
                field_index_path.to_str().unwrap()
            );
            let file = File::open(field_index_path)?;
            let field_indexes: Vec<FieldIndex> = serde_cbor::from_reader(file).map_err(|err| {
                OperationError::service_error(&format!("Unable to load index: {:?}", err))
            })?;

            Ok(field_indexes)
        } else {
            debug!(
                "Index for field `{}` not found in {}, building now",
                field,
                field_index_path.to_str().unwrap()
            );
            let res = self.build_field_index(field, payload_type)?;
            self.save_field_index(field)?;
            Ok(res)
        }
    }

    fn load_all_fields(&mut self) -> OperationResult<()> {
        let mut field_indexes: IndexesMap = Default::default();
        for (field, payload_type) in &self.config.indexed_fields {
            let field_index = self.load_or_build_field_index(field, payload_type.to_owned())?;
            field_indexes.insert(field.clone(), field_index);
        }
        self.field_indexes = field_indexes;
        Ok(())
    }

    pub fn open(
        condition_checker: Arc<ConditionCheckerSS>,
        vector_storage: Arc<AtomicRefCell<VectorStorageSS>>,
        payload: Arc<AtomicRefCell<PayloadStorageSS>>,
        id_tracker: Arc<AtomicRefCell<IdTrackerSS>>,
        path: &Path,
    ) -> OperationResult<Self> {
        create_dir_all(path)?;
        let config_path = PayloadConfig::get_config_path(path);
        let config = if config_path.exists() {
            PayloadConfig::load(&config_path)?
        } else {
            PayloadConfig::default()
        };

        let mut index = StructPayloadIndex {
            condition_checker,
            vector_storage,
            payload,
            id_tracker,
            field_indexes: Default::default(),
            config,
            path: path.to_owned(),
            visited_pool: Default::default(),
        };

        if !index.config_path().exists() {
            // Save default config
            index.save_config()?;
        }

        index.load_all_fields()?;

        Ok(index)
    }

    pub fn build_field_index(
        &self,
        field: PayloadKeyTypeRef,
        field_type: PayloadSchemaType,
    ) -> OperationResult<Vec<FieldIndex>> {
        let payload_storage = self.payload.borrow();

        let mut builders = index_selector(&field_type);
        for point_id in payload_storage.iter_ids() {
            let point_payload = payload_storage.payload(point_id);
            let field_value_opt = point_payload.get_value(field);
            if let Some(field_value) = field_value_opt {
                for builder in &mut builders {
                    builder.add(point_id, field_value);
                }
            }
        }

        let field_indexes = builders
            .iter_mut()
            .map(|builder| builder.build())
            .collect_vec();

        Ok(field_indexes)
    }

    fn build_and_save(
        &mut self,
        field: PayloadKeyTypeRef,
        payload_type: PayloadSchemaType,
    ) -> OperationResult<()> {
        let field_indexes = self.build_field_index(field, payload_type)?;
        self.field_indexes.insert(field.into(), field_indexes);

        self.save_field_index(field)?;

        Ok(())
    }

    pub fn total_points(&self) -> usize {
        self.vector_storage.borrow().vector_count()
    }
}

impl PayloadIndex for StructPayloadIndex {
    fn indexed_fields(&self) -> HashMap<PayloadKeyType, PayloadSchemaType> {
        self.config.indexed_fields.clone()
    }

    fn set_indexed(
        &mut self,
        field: PayloadKeyTypeRef,
        payload_type: PayloadSchemaType,
    ) -> OperationResult<()> {
        if self
            .config
            .indexed_fields
            .insert(field.to_owned(), payload_type)
            .is_none()
        {
            self.save_config()?;
            self.build_and_save(field, payload_type)?;
        }

        Ok(())
    }

    fn drop_index(&mut self, field: PayloadKeyTypeRef) -> OperationResult<()> {
        self.config.indexed_fields.remove(field);
        self.save_config()?;
        self.field_indexes.remove(field);

        let field_index_path = Self::get_field_index_path(&self.path, field);

        if field_index_path.exists() {
            remove_file(&field_index_path)?;
        }

        Ok(())
    }

    fn estimate_cardinality(&self, query: &Filter) -> CardinalityEstimation {
        let total_points = self.total_points();

        let estimator = |condition: &Condition| match condition {
            Condition::Filter(_) => panic!("Unexpected branching"),
            Condition::IsEmpty(IsEmptyCondition { is_empty: field }) => {
                let total_points = self.total_points();

                let mut indexed_points = 0;
                if let Some(field_indexes) = self.field_indexes.get(&field.key) {
                    for index in field_indexes {
                        indexed_points = indexed_points.max(index.count_indexed_points())
                    }
                    CardinalityEstimation {
                        primary_clauses: vec![PrimaryCondition::IsEmpty(IsEmptyCondition {
                            is_empty: field.to_owned(),
                        })],
                        min: 0, // It is possible, that some non-empty payloads are not indexed
                        exp: total_points.saturating_sub(indexed_points), // Expect field type consistency
                        max: total_points.saturating_sub(indexed_points),
                    }
                } else {
                    CardinalityEstimation {
                        primary_clauses: vec![PrimaryCondition::IsEmpty(IsEmptyCondition {
                            is_empty: field.to_owned(),
                        })],
                        min: 0,
                        exp: total_points / 2,
                        max: total_points,
                    }
                }
            }
            Condition::HasId(has_id) => {
                let id_tracker_ref = self.id_tracker.borrow();
                let mapped_ids: HashSet<PointOffsetType> = has_id
                    .has_id
                    .iter()
                    .filter_map(|external_id| id_tracker_ref.internal_id(*external_id))
                    .collect();
                let num_ids = mapped_ids.len();
                CardinalityEstimation {
                    primary_clauses: vec![PrimaryCondition::Ids(mapped_ids)],
                    min: num_ids,
                    exp: num_ids,
                    max: num_ids,
                }
            }
            Condition::Field(field_condition) => self
                .estimate_field_condition(field_condition)
                .unwrap_or_else(|| CardinalityEstimation::unknown(self.total_points())),
        };

        estimate_filter(&estimator, query, total_points)
    }

    fn query_points<'a>(
        &'a self,
        query: &'a Filter,
    ) -> Box<dyn Iterator<Item = PointOffsetType> + 'a> {
        // Assume query is already estimated to be small enough so we can iterate over all matched ids
        let vector_storage_ref = self.vector_storage.borrow();

        let query_cardinality = self.estimate_cardinality(query);
        return if query_cardinality.primary_clauses.is_empty() {
            let full_scan_iterator = vector_storage_ref.iter_ids();
            // Worst case: query expected to return few matches, but index can't be used
            let matched_points = full_scan_iterator
                .filter(|i| self.condition_checker.check(*i, query))
                .collect_vec();

            Box::new(matched_points.into_iter())
        } else {
            // CPU-optimized strategy here: points are made unique before applying other filters.
            // ToDo: Implement iterator which holds the `visited_pool` and borrowed `vector_storage_ref` to prevent `preselected` array creation
            let mut visited_list = self
                .visited_pool
                .get(vector_storage_ref.total_vector_count());

            #[allow(clippy::needless_collect)]
                let preselected: Vec<PointOffsetType> = query_cardinality
                .primary_clauses
                .iter()
                .flat_map(|clause| {
                    match clause {
                        PrimaryCondition::Condition(field_condition) => {
                            self.query_field(field_condition).unwrap_or_else(
                                || vector_storage_ref.iter_ids(), /* index is not built */
                            )
                        }
                        PrimaryCondition::Ids(ids) => Box::new(ids.iter().copied()),
                        PrimaryCondition::IsEmpty(_) => vector_storage_ref.iter_ids() /* there are no fast index for IsEmpty */
                    }
                })
                .filter(|&id| !visited_list.check_and_update_visited(id))
                .filter(move |&i| self.condition_checker.check(i, query))
                .collect();

            self.visited_pool.return_back(visited_list);

            let matched_points_iter = preselected.into_iter();
            Box::new(matched_points_iter)
        };
    }

    fn filter_context<'a>(&'a self, filter: &'a Filter) -> Box<dyn FilterContext + 'a> {
        Box::new(StructFilterContext::new(
            self.condition_checker.clone(),
            filter,
            &self.field_indexes,
            self.estimate_cardinality(filter),
        ))
    }

    fn payload_blocks(
        &self,
        field: PayloadKeyTypeRef,
        threshold: usize,
    ) -> Box<dyn Iterator<Item = PayloadBlockCondition> + '_> {
        match self.field_indexes.get(field) {
            None => Box::new(vec![].into_iter()),
            Some(indexes) => {
                let field_clone = field.to_owned();
                Box::new(indexes.iter().flat_map(move |field_index| {
                    field_index.payload_blocks(threshold, field_clone.clone())
                }))
            }
        }
    }
}

pub struct StructFilterContext<'a> {
    condition_checker: Arc<ConditionCheckerSS>,
    filter: &'a Filter,
    field_indexes: &'a IndexesMap,
    fallback: bool,
    cardinality_estimation: CardinalityEstimation,
}

impl<'a> StructFilterContext<'a> {
    fn new(
        condition_checker: Arc<ConditionCheckerSS>,
        filter: &'a Filter,
        field_indexes: &'a IndexesMap,
        cardinality_estimation: CardinalityEstimation,
    ) -> Self {
        let fallback = check_fallback(&cardinality_estimation.primary_clauses, field_indexes);

        Self {
            condition_checker,
            filter,
            field_indexes,
            cardinality_estimation,
            fallback,
        }
    }

    fn get_field_value(
        &self,
        field_name: PayloadKeyTypeRef,
        point_id: PointOffsetType,
    ) -> Option<Value> {
        match self.field_indexes.get(field_name) {
            Some(indexes) => match &indexes[0] {
                FieldIndex::IntIndex(int_index) => {
                    let values = int_index.get_values(point_id);
                    match values {
                        None => None,
                        Some(v) => {
                            if v.len() == 1 {
                                return Some(Value::Number(v[0].into()));
                            }
                            let values = v
                                .iter()
                                .map(|i| Value::Number(i.to_owned().into()))
                                .collect();
                            Some(Value::Array(values))
                        }
                    }
                }
                FieldIndex::IntMapIndex(int_map_index) => {
                    let values = int_map_index.get_values(point_id);
                    match values {
                        None => None,
                        Some(v) => {
                            if v.len() == 1 {
                                return Some(Value::Number(v[0].into()));
                            }
                            let values = v
                                .iter()
                                .map(|i| Value::Number(i.to_owned().into()))
                                .collect();
                            Some(Value::Array(values))
                        }
                    }
                }
                FieldIndex::KeywordIndex(keyword_index) => {
                    let values = keyword_index.get_values(point_id);
                    match values {
                        None => None,
                        Some(v) => {
                            if v.len() == 1 {
                                return Some(Value::String(v[0].clone()));
                            }
                            let values = v.iter().map(|i| Value::String(i.to_owned())).collect();
                            Some(Value::Array(values))
                        }
                    }
                }

                FieldIndex::FloatIndex(float_index) => {
                    let values = float_index.get_values(point_id);
                    match values {
                        None => None,
                        Some(v) => {
                            if v.len() == 1 {
                                return Some(Value::Number(
                                    serde_json::Number::from_f64(v[0]).unwrap(),
                                ));
                            }
                            let values = v
                                .iter()
                                .map(|i| Value::Number(serde_json::Number::from_f64(*i).unwrap()))
                                .collect();
                            Some(Value::Array(values))
                        }
                    }
                }
                FieldIndex::GeoIndex(geo_index) => {
                    let values = geo_index.get_values(point_id);
                    match values {
                        None => None,
                        Some(v) => {
                            if v.len() == 1 {
                                return Some(build_geo_obj(&v[0]));
                            }
                            let values = v.iter().map(|i| build_geo_obj(i)).collect();
                            Some(Value::Array(values))
                        }
                    }
                }
            },
            None => None,
        }
    }
}

fn build_geo_obj(geo_point: &GeoPoint) -> Value {
    let mut geo_obj = serde_json::Map::new();
    geo_obj.insert(
        "lat".to_string(),
        Value::Number(serde_json::Number::from_f64(geo_point.lat).unwrap()),
    );
    geo_obj.insert(
        "lon".to_string(),
        Value::Number(serde_json::Number::from_f64(geo_point.lon).unwrap()),
    );
    return Value::Object(geo_obj);
}

fn check_fallback(primary_clauses: &[PrimaryCondition], field_indexes: &IndexesMap) -> bool {
    primary_clauses.iter().any(|p| match p {
        PrimaryCondition::Condition(field_condition) => {
            field_indexes.get(&field_condition.key).is_none()
        }
        _ => true,
    })
}

impl<'a> FilterContext for StructFilterContext<'a> {
    fn check(&self, point_id: PointOffsetType) -> bool {
        if self.fallback {
            return self.condition_checker.check(point_id, self.filter);
        }

        let checker = |condition: &Condition| {
            match condition {
                Condition::Field(field_condition) => {
                    self.get_field_value(&field_condition.key, point_id)
                        .map_or(false, |p| {
                            let mut res = false;
                            // ToDo: Convert onto iterator over checkers, so it would be impossible to forget a condition
                            res = res
                                || field_condition
                                    .r#match
                                    .as_ref()
                                    .map_or(false, |condition| condition.check(&p));
                            res = res
                                || field_condition
                                    .range
                                    .as_ref()
                                    .map_or(false, |condition| condition.check(&p));
                            res = res
                                || field_condition
                                    .geo_radius
                                    .as_ref()
                                    .map_or(false, |condition| condition.check(&p));
                            res = res
                                || field_condition
                                    .geo_bounding_box
                                    .as_ref()
                                    .map_or(false, |condition| condition.check(&p));
                            res = res
                                || field_condition
                                    .values_count
                                    .as_ref()
                                    .map_or(false, |condition| condition.check(&p));
                            res
                        })
                }
                _ => panic!("Unexpected condition!"),
            }
        };

        check_filter(&checker, self.filter)
    }
}
