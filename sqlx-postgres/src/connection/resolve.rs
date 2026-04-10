use crate::connection::TableColumns;
use crate::error::Error;
use crate::ext::ustr::UStr;
use crate::message::{ParameterDescription, RowDescription};
use crate::query_as::query_as;
use crate::query_scalar::query_scalar;
use crate::type_info::{PgArrayOf, PgCustomType, PgType, PgTypeKind};
use crate::types::Oid;
use crate::{HashMap, PgRow, PgValueRef, Postgres};
use crate::{PgColumn, PgConnection, PgTypeInfo};
use sqlx_core::column::{ColumnOrigin, TableColumn};
use sqlx_core::database::Database;
use sqlx_core::decode::Decode;
use sqlx_core::error::BoxDynError;
use sqlx_core::from_row::FromRow;
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::row::Row;
use sqlx_core::sql_str::{AssertSqlSafe, SqlSafeStr};
use sqlx_core::types::Type;
use std::fmt::Display;
use std::mem;
use std::ops::ControlFlow;
use std::sync::Arc;
// NOTE: we should only use raw queries in this module because this may occur in the middle
// of an existing extended query flow. Additionally, some third-party implementations don't
// support named prepared statements, so to execute these statements with the extended query flow,
// we'd have to replace the unnamed prepared statement which is already the one the user wanted
// to execute. This means we'd have to immediately re-prepare it, adding an extra round trip.

impl PgConnection {
    pub(super) async fn handle_row_description(
        &mut self,
        desc: Option<RowDescription>,
        fetch_type_info: bool,
        fetch_column_description: bool,
    ) -> Result<(Vec<PgColumn>, HashMap<UStr, usize>), Error> {
        let mut columns = Vec::new();
        let mut column_names = HashMap::new();

        let desc = if let Some(desc) = desc {
            desc
        } else {
            // no rows
            return Ok((columns, column_names));
        };

        columns.reserve(desc.fields.len());
        column_names.reserve(desc.fields.len());

        for (index, field) in desc.fields.into_iter().enumerate() {
            let name = UStr::from(field.name);

            let type_info = self
                .maybe_fetch_type_info_by_oid(field.data_type_id, fetch_type_info)
                .await?;

            let origin = if let (Some(relation_oid), Some(attribute_no)) =
                (field.relation_id, field.relation_attribute_no)
            {
                self.maybe_fetch_column_origin(relation_oid, attribute_no, fetch_column_description)
                    .await?
            } else {
                ColumnOrigin::Expression
            };

            let column = PgColumn {
                ordinal: index,
                name: name.clone(),
                type_info,
                relation_id: field.relation_id,
                relation_attribute_no: field.relation_attribute_no,
                origin,
            };

            columns.push(column);
            column_names.insert(name, index);
        }

        Ok((columns, column_names))
    }

    pub(super) async fn handle_parameter_description(
        &mut self,
        desc: ParameterDescription,
    ) -> Result<Vec<PgTypeInfo>, Error> {
        let mut params = Vec::with_capacity(desc.types.len());

        for ty in desc.types {
            params.push(self.maybe_fetch_type_info_by_oid(ty, true).await?);
        }

        Ok(params)
    }

    async fn maybe_fetch_type_info_by_oid(
        &mut self,
        oid: Oid,
        should_fetch: bool,
    ) -> Result<PgTypeInfo, Error> {
        // first we check if this is a built-in type
        // in the average application, the vast majority of checks should flow through this
        if let Some(info) = PgTypeInfo::try_from_oid(oid) {
            return Ok(info);
        }

        // next we check a local cache for user-defined type names <-> object id
        if let Some(info) = self.inner.cache_type_info.get(&oid) {
            return Ok(info.clone());
        }

        // fallback to asking the database directly for a type name
        if should_fetch {
            // we're boxing this future here so we can use async recursion
            let info = Box::pin(async { self.fetch_type_by_oid(oid).await }).await?;

            // cache the type name <-> oid relationship in a paired hashmap
            // so we don't come down this road again
            self.inner.cache_type_info.insert(oid, info.clone());
            self.inner
                .cache_type_oid
                .insert(info.0.name().to_string().into(), oid);

            Ok(info)
        } else {
            // we are not in a place that *can* run a query
            // this generally means we are in the middle of another query
            // this _should_ only happen for complex types sent through the TEXT protocol
            // we're open to ideas to correct this.. but it'd probably be more efficient to figure
            // out a way to "prime" the type cache for connections rather than make this
            // fallback work correctly for complex user-defined types for the TEXT protocol
            Ok(PgTypeInfo(PgType::DeclareWithOid(oid)))
        }
    }

    async fn maybe_fetch_column_origin(
        &mut self,
        relation_id: Oid,
        attribute_no: i16,
        should_fetch: bool,
    ) -> Result<ColumnOrigin, Error> {
        if let Some(origin) = self
            .inner
            .cache_table_to_column_names
            .get(&relation_id)
            .and_then(|table_columns| {
                let column_name = table_columns.columns.get(&attribute_no).cloned()?;

                Some(ColumnOrigin::Table(TableColumn {
                    table: table_columns.table_name.clone(),
                    name: column_name,
                }))
            })
        {
            return Ok(origin);
        }

        if !should_fetch {
            return Ok(ColumnOrigin::Unknown);
        }

        // Looking up the table name _may_ end up being redundant,
        // but the round-trip to the server is by far the most expensive part anyway.
        let Some((table_name, column_name)): Option<(String, String)> = query_as(
            // language=PostgreSQL
            "SELECT $1::oid::regclass::text, attname \
                 FROM pg_catalog.pg_attribute \
                 WHERE attrelid = $1 AND attnum = $2",
        )
        .bind(relation_id)
        .bind(attribute_no)
        .fetch_optional(&mut *self)
        .await?
        else {
            // The column/table doesn't exist anymore for whatever reason.
            return Ok(ColumnOrigin::Unknown);
        };

        let table_columns = self
            .inner
            .cache_table_to_column_names
            .entry(relation_id)
            .or_insert_with(|| TableColumns {
                table_name: table_name.into(),
                columns: Default::default(),
            });

        let column_name = table_columns
            .columns
            .entry(attribute_no)
            .or_insert(column_name.into());

        Ok(ColumnOrigin::Table(TableColumn {
            table: table_columns.table_name.clone(),
            name: Arc::clone(column_name),
        }))
    }

    async fn fetch_type_by_oid(&mut self, oid: Oid) -> Result<PgTypeInfo, Error> {
        let (name, typ_type, category, relation_id, element, base_type): (
            String,
            i8,
            i8,
            Oid,
            Oid,
            Oid,
        ) = query_as(
            // Converting the OID to `regtype` and then `text` will give us the name that
            // the type will need to be found at by search_path.
            "SELECT oid::regtype::text, \
                     typtype, \
                     typcategory, \
                     typrelid, \
                     typelem, \
                     typbasetype \
                     FROM pg_catalog.pg_type \
                     WHERE oid = $1",
        )
        .bind(oid)
        .fetch_one(&mut *self)
        .await?;

        let typ_type = TypType::try_from(typ_type);
        let category = TypCategory::try_from(category);

        match (typ_type, category) {
            (Ok(TypType::Domain), _) => self.fetch_domain_by_oid(oid, base_type, name).await,

            (Ok(TypType::Base), Ok(TypCategory::Array)) => {
                Ok(PgTypeInfo(PgType::Custom(Arc::new(PgCustomType {
                    kind: PgTypeKind::Array(
                        self.maybe_fetch_type_info_by_oid(element, true).await?,
                    ),
                    name: name.into(),
                    oid,
                }))))
            }

            (Ok(TypType::Pseudo), Ok(TypCategory::Pseudo)) => {
                Ok(PgTypeInfo(PgType::Custom(Arc::new(PgCustomType {
                    kind: PgTypeKind::Pseudo,
                    name: name.into(),
                    oid,
                }))))
            }

            (Ok(TypType::Range), Ok(TypCategory::Range)) => {
                self.fetch_range_by_oid(oid, name).await
            }

            (Ok(TypType::Enum), Ok(TypCategory::Enum)) => self.fetch_enum_by_oid(oid, name).await,

            (Ok(TypType::Composite), Ok(TypCategory::Composite)) => {
                self.fetch_composite_by_oid(oid, relation_id, name).await
            }

            _ => Ok(PgTypeInfo(PgType::Custom(Arc::new(PgCustomType {
                kind: PgTypeKind::Simple,
                name: name.into(),
                oid,
            })))),
        }
    }

    async fn fetch_enum_by_oid(&mut self, oid: Oid, name: String) -> Result<PgTypeInfo, Error> {
        let variants: Vec<String> = query_scalar(
            r#"
SELECT enumlabel
FROM pg_catalog.pg_enum
WHERE enumtypid = $1
ORDER BY enumsortorder
            "#,
        )
        .bind(oid)
        .fetch_all(self)
        .await?;

        Ok(PgTypeInfo(PgType::Custom(Arc::new(PgCustomType {
            oid,
            name: name.into(),
            kind: PgTypeKind::Enum(Arc::from(variants)),
        }))))
    }

    async fn fetch_composite_by_oid(
        &mut self,
        oid: Oid,
        relation_id: Oid,
        name: String,
    ) -> Result<PgTypeInfo, Error> {
        let raw_fields: Vec<(String, Oid)> = query_as(
            r#"
SELECT attname, atttypid
FROM pg_catalog.pg_attribute
WHERE attrelid = $1
AND NOT attisdropped
AND attnum > 0
ORDER BY attnum
                "#,
        )
        .bind(relation_id)
        .fetch_all(&mut *self)
        .await?;

        let mut fields = Vec::new();

        for (field_name, field_oid) in raw_fields.into_iter() {
            let field_type = self.maybe_fetch_type_info_by_oid(field_oid, true).await?;

            fields.push((field_name, field_type));
        }

        Ok(PgTypeInfo(PgType::Custom(Arc::new(PgCustomType {
            oid,
            name: name.into(),
            kind: PgTypeKind::Composite(Arc::from(fields)),
        }))))
    }

    async fn fetch_domain_by_oid(
        &mut self,
        oid: Oid,
        base_type: Oid,
        name: String,
    ) -> Result<PgTypeInfo, Error> {
        let base_type = self.maybe_fetch_type_info_by_oid(base_type, true).await?;

        Ok(PgTypeInfo(PgType::Custom(Arc::new(PgCustomType {
            oid,
            name: name.into(),
            kind: PgTypeKind::Domain(base_type),
        }))))
    }

    async fn fetch_range_by_oid(&mut self, oid: Oid, name: String) -> Result<PgTypeInfo, Error> {
        let element_oid: Oid = query_scalar(
            r#"
SELECT rngsubtype
FROM pg_catalog.pg_range
WHERE rngtypid = $1
                "#,
        )
        .bind(oid)
        .fetch_one(&mut *self)
        .await?;

        let element = self.maybe_fetch_type_info_by_oid(element_oid, true).await?;

        Ok(PgTypeInfo(PgType::Custom(Arc::new(PgCustomType {
            kind: PgTypeKind::Range(element),
            name: name.into(),
            oid,
        }))))
    }

    pub(crate) async fn argument_types_to_oids(
        &mut self,
        types: &[PgTypeInfo],
    ) -> Result<Vec<Oid>, Error> {
        let mut oids = Vec::with_capacity(types.len());

        let mut unresolved_types = types.iter();

        for ty in &mut unresolved_types {
            let Some(oid) = self.try_type_to_oid(ty) else {
                break;
            };

            oids.push(oid);
        }

        // Fast-path
        if oids.len() == types.len() {
            return Ok(oids);
        }

        let mut resolver = TypesResolver::default();

        for ty in unresolved_types.clone() {
            if let Some(_) = self.try_type_to_oid(ty) {
                continue;
            }

            resolver.push_type(
                // `escape_default()` should produce a valid SQL string literal
                // https://doc.rust-lang.org/stable/std/primitive.char.html#method.escape_default
                // https://www.postgresql.org/docs/current/sql-syntax-lexical.html#SQL-SYNTAX-STRINGS-ESCAPE
                //
                // `to_regtype()` evaluates to `NULL` if the type does not exist,
                // instead of throwing an exception
                format_args!("to_regtype('{}')::oid", ty.name().escape_default()),
            );
        }

        resolver.fill_cache(self).await?;

        for ty in unresolved_types {
            let Some(oid) = self.try_type_to_oid(ty) else {
                return Err(Error::TypeNotFound {
                    type_name: ty.name().to_string(),
                });
            };

            oids.push(oid);
        }

        Ok(oids)
    }

    fn try_type_to_oid(&self, ty: &PgTypeInfo) -> Option<Oid> {
        if let Some(oid) = ty.try_oid() {
            return Some(oid);
        }

        match &ty.0 {
            PgType::DeclareWithName(name) => self.inner.cache_type_oid.get(name).copied(),
            PgType::DeclareArrayOf(array) => {
                self.inner.cache_type_oid.get(&array.elem_name).copied()
            }
            // `.try_oid()` should return `Some()` or it should be covered here
            _ => unreachable!("(bug) OID should be resolvable for type {ty:?}"),
        }
    }

    pub(crate) async fn resolve_type_id(&mut self, ty: &PgType) -> Result<Oid, Error> {
        if let Some(oid) = ty.try_oid() {
            return Ok(oid);
        }

        match ty {
            PgType::DeclareWithName(name) => self.fetch_type_id_by_name(name).await,
            PgType::DeclareArrayOf(array) => self.fetch_array_type_id(array).await,
            // `.try_oid()` should return `Some()` or it should be covered here
            _ => unreachable!("(bug) OID should be resolvable for type {ty:?}"),
        }
    }

    pub(crate) async fn fetch_type_id_by_name(&mut self, name: &str) -> Result<Oid, Error> {
        if let Some(oid) = self.inner.cache_type_oid.get(name) {
            return Ok(*oid);
        }

        // language=SQL
        let (oid,): (Oid,) = query_as("SELECT $1::regtype::oid")
            .bind(name)
            .fetch_optional(&mut *self)
            .await?
            .ok_or_else(|| Error::TypeNotFound {
                type_name: name.into(),
            })?;

        self.inner
            .cache_type_oid
            .insert(name.to_string().into(), oid);
        Ok(oid)
    }

    pub(crate) async fn fetch_array_type_id(&mut self, array: &PgArrayOf) -> Result<Oid, Error> {
        if let Some(oid) = self
            .inner
            .cache_type_oid
            .get(&array.elem_name)
            .and_then(|elem_oid| self.inner.cache_elem_type_to_array.get(elem_oid))
        {
            return Ok(*oid);
        }

        // language=SQL
        let (elem_oid, array_oid): (Oid, Oid) =
            query_as("SELECT oid, typarray FROM pg_catalog.pg_type WHERE oid = $1::regtype::oid")
                .bind(&*array.elem_name)
                .fetch_optional(&mut *self)
                .await?
                .ok_or_else(|| Error::TypeNotFound {
                    type_name: array.name.to_string(),
                })?;

        // Avoids copying `elem_name` until necessary
        self.inner
            .cache_type_oid
            .entry_ref(&array.elem_name)
            .insert(elem_oid);
        self.inner
            .cache_elem_type_to_array
            .insert(elem_oid, array_oid);

        Ok(array_oid)
    }

    fn try_type_by_oid(&self, oid: Oid) -> Option<PgTypeInfo> {
        PgTypeInfo::try_from_oid(oid).or_else(|| self.inner.cache_type_info.get(&oid).cloned())
    }

    fn try_cache_type(&mut self, ty: &TypeResolverRow) -> Result<ControlFlow<Oid>, Error> {
        if self.try_type_by_oid(ty.oid).is_some() {
            return Ok(ControlFlow::Continue(()));
        }

        let custom_type_kind = match (ty.typtype, ty.typcategory) {
            (TypType::Domain, _) => {
                let typbasetype = ty.typbasetype.ok_or_else(|| {
                    err_protocol!(
                        "type category is listed as domain, but no base type was found: {ty:?}"
                    )
                })?;

                let Some(base_type) = self.try_type_by_oid(typbasetype) else {
                    return Ok(ControlFlow::Break(typbasetype));
                };

                PgTypeKind::Domain(base_type)
            }

            (TypType::Base, TypCategory::Array) => {
                let typelem = ty.typelem.ok_or_else(|| {
                    err_protocol!(
                        "type category is listed as array, but no element type was found: {ty:?}"
                    )
                })?;

                let Some(elem_type) = self.try_type_by_oid(typelem) else {
                    return Ok(ControlFlow::Break(typelem));
                };

                PgTypeKind::Array(elem_type)
            }

            (TypType::Pseudo, _) => PgTypeKind::Pseudo,

            (TypType::Range, _) => {
                let rngsubtype = ty.rngsubtype.ok_or_else(|| {
                    err_protocol!(
                        "type category is listed as range, but no subtype was found: {ty:?}"
                    )
                })?;

                let Some(sub_type) = self.try_type_by_oid(rngsubtype) else {
                    return Ok(ControlFlow::Break(rngsubtype));
                };

                PgTypeKind::Range(sub_type)
            }

            (TypType::Enum, _) => PgTypeKind::Enum(ty.enum_labels.iter().cloned().collect()),

            (TypType::Composite, _) => {
                let mut attributes = Vec::with_capacity(ty.record_attributes.len());

                for (name, oid) in &ty.record_attributes {
                    let Some(attribute_type) = self.try_type_by_oid(*oid) else {
                        return Ok(ControlFlow::Break(*oid));
                    };

                    attributes.push((name.clone(), attribute_type));
                }

                PgTypeKind::Composite(attributes.into())
            }

            _ => PgTypeKind::Simple,
        };

        let typname = UStr::new(&ty.typname);

        self.inner
            .cache_type_oid
            .entry_ref(&typname)
            .or_insert(ty.oid);

        self.inner.cache_type_info.entry(ty.oid).or_insert_with(|| {
            PgTypeInfo(PgType::Custom(Arc::new(PgCustomType {
                kind: custom_type_kind,
                name: typname.clone(),
                oid: ty.oid,
            })))
        });

        Ok(ControlFlow::Continue(()))
    }
}

#[derive(Default)]
struct TypesResolver {
    query: String,
}

impl TypesResolver {
    fn push_type(&mut self, oid_expr: impl Display) {
        use std::fmt::Write;

        // Lazily push the preamble to `self.query` so we don't allocate in the fast path
        // (all types already known)
        if self.query.is_empty() {
            write!(
                &mut self.query,
                "SELECT pg_type.oid,\n\
                     pg_type.oid::regtype::text typname,\n\
                     typtype,\n\
                     typcategory,\n\
                     typrelid,\n\
                     typelem,\n\
                     typbasetype,\n\
                     rngsubtype,\n\
                     (SELECT array_agg(enumlabel)\n\
                      FROM pg_catalog.pg_enum\n\
                      WHERE enumtypid = pg_type.oid\n\
                      ORDER BY enumsortorder) enum_labels,\n\
                     (SELECT array_agg((attname, atttypid))\n\
                      FROM pg_catalog.pg_attribute\n\
                      WHERE attrelid = pg_type.oid\n\
                        AND NOT attisdropped\n\
                        AND attnum > 0\n\
                      ORDER BY attnum) record_attributes\n\
                 FROM pg_catalog.pg_type\n\
                 LEFT JOIN pg_catalog.pg_range ON pg_type.oid = pg_range.oid\n\
                 WHERE oid IN ({oid_expr}"
            )
            .expect("error writing type expression to query string")
        } else {
            write!(&mut self.query, ", {oid_expr}")
                .expect("error writing type expression to query string")
        }
    }

    async fn fill_cache(&mut self, conn: &mut PgConnection) -> Result<(), Error> {
        let mut existing_dependencies = HashMap::<Oid, Vec<TypeResolverRow>>::new();

        // Iteratively resolve types until all or resolved or we hit a dead-end
        while !self.query.is_empty() {
            // * Cancel-safety
            // * Makes this type reusable if we want to for whatever reason
            // * Avoids an allocation when converting to `SqlStr`
            let mut query = mem::take(&mut self.query);
            query.push(')');

            let types = raw_sql(AssertSqlSafe(query)).fetch_all(&mut *conn).await?;

            let mut new_dependencies = HashMap::<Oid, Vec<TypeResolverRow>>::new();

            'outer: for row in types {
                let mut type_row = TypeResolverRow::from_row(&row)?;

                let mut dependent_types = existing_dependencies
                    .remove(&type_row.oid)
                    .unwrap_or_default();

                loop {
                    if let ControlFlow::Break(missing_oid) = conn.try_cache_type(&type_row)? {
                        if !dependent_types.is_empty() {
                            new_dependencies
                                .entry(type_row.oid)
                                .or_default()
                                .extend(dependent_types);
                        }

                        new_dependencies
                            .entry(missing_oid)
                            .or_default()
                            .push(type_row);

                        self.push_type(missing_oid.0);

                        continue 'outer;
                    }

                    if let Some(next_row) = dependent_types.pop() {
                        type_row = next_row
                    } else {
                        break;
                    }
                }
            }

            if !existing_dependencies.is_empty() {
                return Err(Error::Protocol(format!(
                    "unable to resolve type OIDs: {:?}",
                    existing_dependencies.keys()
                )));
            }

            existing_dependencies = new_dependencies;
        }

        Ok(())
    }
}

#[derive(Debug)]
struct TypeResolverRow {
    oid: Oid,
    typname: String,
    typtype: TypType,
    typcategory: TypCategory,
    typrelid: Option<Oid>,
    typelem: Option<Oid>,
    typbasetype: Option<Oid>,
    rngsubtype: Option<Oid>,
    enum_labels: Vec<String>,
    record_attributes: Vec<(String, Oid)>,
}

// Can't use `#[derive(FromRow)]` here
impl<'r> FromRow<'r, PgRow> for TypeResolverRow {
    fn from_row(row: &'r PgRow) -> Result<Self, Error> {
        Ok(Self {
            oid: row.try_get("oid")?,
            typname: row.try_get("typname")?,
            typtype: row.try_get("typtype")?,
            typcategory: row.try_get("typcategory")?,
            typrelid: row.try_get("typrelid")?,
            typelem: row.try_get("typelem")?,
            typbasetype: row.try_get("typbasetype")?,
            rngsubtype: row.try_get("rngsubtype")?,
            enum_labels: row.try_get("enum_labels")?,
            record_attributes: row.try_get("record_attributes")?,
        })
    }
}

/// Describes the type of the `pg_type.typtype` column
///
/// See <https://www.postgresql.org/docs/13/catalog-pg-type.html>
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TypType {
    Base,
    Composite,
    Domain,
    Enum,
    Pseudo,
    Range,
}

impl TryFrom<i8> for TypType {
    type Error = String;

    fn try_from(t: i8) -> Result<Self, Self::Error> {
        let t = u8::try_from(t).map_err(|_| format!("unknown type code {t}"))?;

        let t = match t {
            b'b' => Self::Base,
            b'c' => Self::Composite,
            b'd' => Self::Domain,
            b'e' => Self::Enum,
            b'p' => Self::Pseudo,
            b'r' => Self::Range,
            _ => return Err(format!("unknown type code {t}")),
        };
        Ok(t)
    }
}

impl<'r> Decode<'r, Postgres> for TypType {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(i8::decode(value)?.try_into()?)
    }
}

impl Type<Postgres> for TypType {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo(PgType::Char)
    }
}

/// Describes the type of the `pg_type.typcategory` column
///
/// See <https://www.postgresql.org/docs/13/catalog-pg-type.html#CATALOG-TYPCATEGORY-TABLE>
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TypCategory {
    Array,
    Boolean,
    Composite,
    DateTime,
    Enum,
    Geometric,
    Network,
    Numeric,
    Pseudo,
    Range,
    String,
    Timespan,
    User,
    BitString,
    Unknown,
}

impl TryFrom<i8> for TypCategory {
    type Error = String;

    fn try_from(c: i8) -> Result<Self, Self::Error> {
        let c = u8::try_from(c).map_err(|_| format!("invalid category code {c}"))?;

        let c = match c {
            b'A' => Self::Array,
            b'B' => Self::Boolean,
            b'C' => Self::Composite,
            b'D' => Self::DateTime,
            b'E' => Self::Enum,
            b'G' => Self::Geometric,
            b'I' => Self::Network,
            b'N' => Self::Numeric,
            b'P' => Self::Pseudo,
            b'R' => Self::Range,
            b'S' => Self::String,
            b'T' => Self::Timespan,
            b'U' => Self::User,
            b'V' => Self::BitString,
            b'X' => Self::Unknown,
            _ => return Err(format!("invalid category code {c}")),
        };
        Ok(c)
    }
}

impl<'r> Decode<'r, Postgres> for TypCategory {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(i8::decode(value)?.try_into()?)
    }
}

impl Type<Postgres> for TypCategory {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo(PgType::Char)
    }
}
