use crate::notebook::db::EmbedStoreError::Runtime;
use arrow_array::types::Float32Type;
use arrow_array::{
    ArrayRef, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray,
};
use arrow_schema::{ArrowError, DataType, Field, Schema};
use fastembed::{Embedding, TextEmbedding};
use futures::TryStreamExt;
use lancedb::connection::Connection;
use lancedb::index::Index;
use lancedb::{connect, Table};
use serde::Serialize;
use std::error::Error;
use std::fmt;
use std::fmt::Formatter;
use std::sync::Arc;

const DB_DIR: &str = "../data";
const DB_NAME: &str = "sample-lancedb";
const TABLE_NAME: &str = "documents";
const EMBEDDING_DIMENSIONS: usize = 384;
const COLUMN_ID: &str = "id";
const COLUMN_EMBEDDINGS: &str = "embeddings";
const COLUMN_TEXT: &str = "text";

#[derive(Debug, Clone, Serialize)]
pub struct Document {
    pub id: String,
    pub text: String,
}

impl PartialEq for Document {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

pub struct EmbedStore {
    embedding_model: TextEmbedding,
    db_conn: Connection,
    table: Table,
}

#[derive(Debug)]
pub enum EmbedStoreError {
    VectorDb(lancedb::error::Error),
    Arrow(ArrowError),
    Embedding(anyhow::Error),
    Runtime(String),
}

impl fmt::Display for EmbedStoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            EmbedStoreError::VectorDb(e) => write!(f, "{}", e),
            EmbedStoreError::Arrow(e) => write!(f, "{}", e),
            EmbedStoreError::Embedding(e) => write!(f, "{}", e),
            Runtime(e) => write!(f, "{}", e),
        }
    }
}

impl Error for EmbedStoreError {
    // Implement this to return the lower level source of this Error.
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            EmbedStoreError::VectorDb(e) => Some(e),
            EmbedStoreError::Arrow(e) => Some(e),
            EmbedStoreError::Embedding(_) => None,
            Runtime(_) => None,
        }
    }
}

impl From<lancedb::error::Error> for EmbedStoreError {
    fn from(e: lancedb::error::Error) -> Self {
        EmbedStoreError::VectorDb(e)
    }
}

impl From<ArrowError> for EmbedStoreError {
    fn from(e: ArrowError) -> Self {
        EmbedStoreError::Arrow(e)
    }
}

impl From<anyhow::Error> for EmbedStoreError {
    fn from(e: anyhow::Error) -> Self {
        EmbedStoreError::Embedding(e)
    }
}

impl EmbedStore {
    pub async fn new(embedding_model: TextEmbedding) -> Result<EmbedStore, EmbedStoreError> {
        let db_conn = Self::init_db_conn().await?;
        let table = Self::get_or_create_table(&db_conn, TABLE_NAME).await?;
        Ok(EmbedStore {
            embedding_model,
            db_conn,
            table,
        })
    }

    pub async fn add(
        &self,
        alt_ids: Vec<String>,
        text: Vec<String>,
    ) -> Result<(), EmbedStoreError> {
        log::info!("Saving Documents: {:?}", alt_ids);
        let embeddings = self.create_embeddings(&text)?;
        assert_eq!(
            embeddings[0].len(),
            EMBEDDING_DIMENSIONS,
            "Embedding dimensions mismatch"
        );
        let schema = self.table.schema().await?;
        let records_iter = self
            .create_record_batch(embeddings, text, alt_ids, schema.clone())
            .into_iter()
            .map(Ok);

        let batches = RecordBatchIterator::new(records_iter, schema.clone());
        self.table
            .add(Box::new(batches))
            .execute()
            .await
            .map_err(EmbedStoreError::from)
    }

    pub async fn record_count(&self) -> Result<usize, EmbedStoreError> {
        self.table
            .count_rows(None)
            .await
            .map_err(EmbedStoreError::from)
    }

    pub async fn search(
        &self,
        search_text: &str,
        filter: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<(Document, f32)>, EmbedStoreError> {
        let limit = limit.unwrap_or(25);
        let query = self.create_embeddings(&[search_text.to_string()])?;
        // flattening a 2D vector into a 1D vector. This is necessary because the search
        // function of the Table trait expects a 1D vector as input. However, the
        // create_embeddings function returns a 2D vector (a vector of embeddings,
        // where each embedding is itself a vector)
        let query: Vec<f32> = query
            .into_iter()
            .flat_map(|embedding| embedding.to_vec())
            .collect();
        self.execute_search(query, filter, Some(limit)).await
    }

    async fn execute_search(
        &self,
        query: Vec<f32>,
        filter: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<(Document, f32)>, EmbedStoreError> {
        let limit = limit.unwrap_or(10);
        let mut query_builder = self.table.search(&query).limit(limit);
        // filter, see https://lancedb.github.io/lancedb/sql/
        if let Some(filter_clause) = filter {
            query_builder = query_builder.filter(filter_clause);
        }

        let stream = query_builder.execute_stream().await?;
        let record_batches = match stream.try_collect::<Vec<_>>().await {
            Ok(batches) => batches,
            Err(err) => {
                return Err(EmbedStoreError::VectorDb(lancedb::error::Error::Runtime {
                    message: err.to_string(),
                }));
            }
        };
        let documents = self.record_to_document_with_distances(record_batches)?;

        Ok(documents)
    }

    async fn execute_query(
        &self,
        query: Option<Vec<f32>>,
        filter: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<Document>, EmbedStoreError> {
        let limit = limit.unwrap_or(10);
        let mut query_builder = match query {
            None => self.table.query().limit(limit),
            Some(search_values) => self.table.search(&search_values).limit(limit),
        };
        if let Some(filter_clause) = filter {
            query_builder = query_builder.filter(filter_clause);
        }
        let stream = query_builder.execute_stream().await?;

        let record_batches = match stream.try_collect::<Vec<_>>().await {
            Ok(batches) => batches,
            Err(err) => {
                return Err(EmbedStoreError::VectorDb(lancedb::error::Error::Runtime {
                    message: err.to_string(),
                }));
            }
        };

        let documents = self.record_to_document(record_batches)?;
        Ok(documents)
    }
    pub async fn get(&self, id: &str) -> Result<Option<Document>, EmbedStoreError> {
        let filter = format!("id = '{}'", id);
        let mut result = self.execute_query(None, Some(&filter), None).await?;
        assert!(
            result.len() <= 1,
            "The get by id method should only return one item at most"
        );
        Ok(result.pop())
    }

    pub async fn get_all(&self) -> Result<(Vec<Document>, usize), EmbedStoreError> {
        let total_records = self.record_count().await?;
        let documents = self.execute_query(None, None, Some(1000)).await?;
        log::info!(
            "get_all returned {} records. Total rows in db: {}",
            documents.len(),
            total_records
        );

        Ok((documents, total_records))
    }

    pub async fn delete<T: fmt::Display>(&self, ids: &Vec<T>) -> Result<(), EmbedStoreError> {
        let comma_separated = ids
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<String>>()
            .join(", ");
        self.table
            .delete(format!("id in ('{}')", comma_separated).as_str())
            .await
            .map_err(EmbedStoreError::from)
    }

    pub async fn update(&self, id: &str, text: &str) -> Result<(), EmbedStoreError> {
        self.delete(&vec![id]).await?;
        self.add(vec![id.to_string()], vec![text.to_string()]).await
    }

    /// Creates an index on a given field.
    pub async fn create_index(&self) -> Result<(), EmbedStoreError> {
        self.table
            .create_index(&[COLUMN_EMBEDDINGS], Index::Auto)
            .execute()
            .await
            .map_err(EmbedStoreError::from)
    }

    fn record_to_document_with_distances(
        &self,
        record_batches: Vec<RecordBatch>,
    ) -> Result<Vec<(Document, f32)>, EmbedStoreError> {
        let mut docs_with_distance: Vec<(Document, f32)> = Vec::new();
        if record_batches.is_empty() {
            return Ok(vec![]);
        }
        for record_batch in record_batches {
            let ids = self.downcast_column::<StringArray>(&record_batch, "id")?;
            let texts = self.downcast_column::<StringArray>(&record_batch, "text")?;
            let distances = self.downcast_column::<Float32Array>(&record_batch, "_distance")?;

            (0..record_batch.num_rows()).for_each(|index| {
                let id = ids.value(index).to_string();
                let text = texts.value(index).to_string();
                let distance = distances.value(index);
                docs_with_distance.push((Document { id: id, text: text }, distance))
            });
        }
        log::info!(
            "Converted [{}] batch results to Documents",
            docs_with_distance.len()
        );

        Ok(docs_with_distance)
    }

    fn record_to_document(
        &self,
        record_batches: Vec<RecordBatch>,
    ) -> Result<Vec<Document>, EmbedStoreError> {
        let mut documents: Vec<Document> = Vec::new();
        if record_batches.is_empty() {
            return Ok(vec![]);
        }
        for record_batch in record_batches {
            let ids = self.downcast_column::<StringArray>(&record_batch, "id")?;
            let texts = self.downcast_column::<StringArray>(&record_batch, "text")?;

            (0..record_batch.num_rows()).for_each(|index| {
                let id = ids.value(index).to_string();
                let text = texts.value(index).to_string();
                documents.push(Document { id: id, text: text })
            });
        }
        log::info!("Converted [{}] batch results to Documents", documents.len());
        Ok(documents)
    }

    async fn init_db_conn() -> Result<Connection, EmbedStoreError> {
        let db_path = format!("{}/{}", DB_DIR, DB_NAME);
        log::info!("Connecting to db at path: '{}'", db_path);
        let db_conn = connect(db_path.as_str()).execute().await?;
        Ok(db_conn)
    }

    fn create_embeddings(&self, documents: &[String]) -> Result<Vec<Embedding>, EmbedStoreError> {
        self.embedding_model
            .embed(documents.to_vec(), None)
            .map_err(EmbedStoreError::from)
    }

    /// Transforms a 2D vector into a 2D vector where each element is wrapped in an `Option`.
    ///
    /// This function takes a 2D vector `source` as input and returns a new 2D vector where each element
    /// is wrapped in an `Option`.
    /// The outer vector is also wrapped in an `Option`. This is useful when you want to represent the
    /// absence of data in your vector.
    ///
    /// # Arguments
    ///
    /// * `source` - A 2D vector that will be transformed.
    ///
    /// # Returns
    ///
    /// A 2D vector where each element is wrapped in an `Option`, and the outer vector is also wrapped in an `Option`.
    ///
    /// # Example
    ///
    /// ```
    /// let source = vec![vec![1, 2, 3], vec![4, 5, 6]];
    /// let result = wrap_in_option(source);
    /// assert_eq!(result, vec![Some(vec![Some(1), Some(2), Some(3)]), Some(vec![Some(4), Some(5), Some(6)])]);
    /// ```
    fn wrap_in_option<T>(&self, source: Vec<Vec<T>>) -> Vec<Option<Vec<Option<T>>>> {
        source
            .into_iter()
            .map(|inner_vec| Some(inner_vec.into_iter().map(|item| Some(item)).collect()))
            .collect()
    }

    /// Creates a record batch from a list of embeddings and a correlated list of original text.
    fn create_record_batch(
        &self,
        embeddings: Vec<Vec<f32>>,
        text: Vec<String>,
        alt_ids: Vec<String>,
        schema: Arc<Schema>,
    ) -> Vec<RecordBatch> {
        let dimensions_count = embeddings[0].len();
        let wrapped_source = self.wrap_in_option(embeddings);
        let record_batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                // id field
                // Arc::new(Int32Array::from_iter_values(0..total_records_count as i32)),
                // Embeddings field
                Arc::new(
                    FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                        wrapped_source,
                        dimensions_count as i32,
                    ),
                ),
                // Text field
                Arc::new(Arc::new(StringArray::from(text)) as ArrayRef),
                // Alt Id
                Arc::new(Arc::new(StringArray::from(alt_ids)) as ArrayRef),
            ],
        );
        match record_batch {
            Ok(batch) => {
                vec![batch]
            }
            Err(e) => {
                panic!("Was unable to create a record batch: {}", e)
            }
        }
    }

    fn generate_schema(dimensions_count: usize) -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new(
                COLUMN_EMBEDDINGS,
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    dimensions_count as i32,
                ),
                true,
            ),
            Field::new(COLUMN_TEXT, DataType::Utf8, false),
            Field::new(COLUMN_ID, DataType::Utf8, false),
        ]))
    }

    async fn get_or_create_table(db_conn: &Connection, table_name: &str) -> lancedb::Result<Table> {
        let table_names = db_conn.table_names().execute().await?;
        log::info!("Existing tables: {:?}", table_names);
        let table = table_names.iter().find(|&name| name == table_name);
        match table {
            Some(_) => {
                log::info!("Connecting to existing table '{}'", table_name);
                let table = db_conn.open_table(table_name).execute().await?;
                Ok(table)
            }
            None => {
                log::info!("Table '{}' not found, creating new table", table_name);
                let schema = Self::generate_schema(EMBEDDING_DIMENSIONS);
                let batches = RecordBatchIterator::new(vec![], schema.clone());
                let table = db_conn
                    .create_table(table_name, Box::new(batches))
                    .execute()
                    .await?;
                Ok(table)
            }
        }
    }

    /// Creates an empty table with a schema.
    async fn create_empty_table(
        &self,
        table_name: &str,
        dimensions_count: usize,
    ) -> lancedb::Result<Table> {
        let schema = Self::generate_schema(dimensions_count);
        let batches = RecordBatchIterator::new(vec![], schema.clone());
        self.db_conn
            .create_table(table_name, Box::new(batches))
            .execute()
            .await
    }

    fn downcast_column<'a, T: std::fmt::Debug + 'static>(
        &self,
        record_batch: &'a RecordBatch,
        column_name: &str,
    ) -> Result<&'a T, EmbedStoreError> {
        record_batch
            .column_by_name(column_name)
            .ok_or_else(|| EmbedStoreError::Runtime(format!("{} column not found", column_name)))?
            .as_any()
            .downcast_ref::<T>()
            .ok_or_else(|| {
                EmbedStoreError::Runtime(format!("Failed downcasting {} column", column_name))
            })
    }
}
