#[derive(Debug, Clone)]
pub struct MetricDelta {
    pub category: String,
    pub item: String,
    pub ok: u64,
    pub err: u64,
}

#[derive(Debug, Clone)]
pub struct MetricAgg {
    pub category: String,
    pub item: String,
    pub ok_count: i64,
    pub err_count: i64,
}
