use super::ArgumentRepairLimit;
use super::ArgumentRepairLimits;
use super::EngineError;
use super::UnsupportedSchemaReason;
use super::pointer::JsonPointer;
use crate::AdditionalProperties;
use crate::JsonSchema;
use serde::Serialize;
use std::collections::BTreeSet;
use std::io;

pub(crate) struct SchemaGraph<'a> {
    root: &'a JsonSchema,
    limits: &'a ArgumentRepairLimits,
}

pub(crate) struct ResolvedSchema<'a> {
    pub(crate) schema: &'a JsonSchema,
    pub(crate) key: String,
}

impl<'a> SchemaGraph<'a> {
    pub(crate) fn new(root: &'a JsonSchema, limits: &'a ArgumentRepairLimits) -> Self {
        Self { root, limits }
    }

    pub(crate) fn root(&self) -> &'a JsonSchema {
        self.root
    }

    pub(crate) fn limits(&self) -> &ArgumentRepairLimits {
        self.limits
    }

    pub(crate) fn preflight(&self) -> Result<(), EngineError> {
        let mut state = TraversalState::default();
        self.visit(self.root, /*depth*/ 0, &mut state)
    }

    pub(crate) fn resolve(&self, schema_ref: &str) -> Result<ResolvedSchema<'a>, EngineError> {
        let fragment = schema_ref
            .strip_prefix('#')
            .ok_or(EngineError::Unsupported(
                UnsupportedSchemaReason::ExternalReference,
            ))?;
        let decoded = urlencoding::decode(fragment)
            .map_err(|_| EngineError::Unsupported(UnsupportedSchemaReason::InvalidReference))?;
        let pointer = JsonPointer::parse(decoded.as_ref()).ok_or(EngineError::Unsupported(
            UnsupportedSchemaReason::InvalidReference,
        ))?;
        let schema = self
            .schema_at(pointer.tokens())
            .ok_or(EngineError::Unsupported(
                UnsupportedSchemaReason::MissingReference,
            ))?;
        Ok(ResolvedSchema {
            schema,
            key: pointer.as_string(),
        })
    }

    fn visit(
        &self,
        schema: &'a JsonSchema,
        depth: usize,
        state: &mut TraversalState,
    ) -> Result<(), EngineError> {
        if depth > self.limits.max_schema_depth {
            return Err(EngineError::Limit(ArgumentRepairLimit::SchemaDepth));
        }
        state.nodes = state.nodes.saturating_add(1);
        if state.nodes > self.limits.max_schema_nodes {
            return Err(EngineError::Limit(ArgumentRepairLimit::SchemaNodes));
        }
        state.add_schema_bytes(/*bytes*/ 1, self.limits)?;

        if let Some(schema_ref) = schema.schema_ref.as_deref() {
            state.add_metadata(schema_ref.len(), self.limits)?;
            state.references = state.references.saturating_add(1);
            if state.references > self.limits.max_references {
                return Err(EngineError::Limit(ArgumentRepairLimit::References));
            }
            let resolved = self.resolve(schema_ref)?;
            if depth >= self.limits.max_schema_depth {
                return Err(EngineError::Limit(ArgumentRepairLimit::SchemaDepth));
            }
            if state.active_refs.contains(&resolved.key) {
                return Err(EngineError::Unsupported(
                    UnsupportedSchemaReason::ReferenceCycle,
                ));
            }
            // Do not memoize completed references without their traversal depth. A shared
            // subtree that is harmless at a shallow site can exceed the depth bound when reused
            // below a deeper property or composition branch.
            state.active_refs.insert(resolved.key.clone());
            self.visit(resolved.schema, depth + 1, state)?;
            state.active_refs.remove(&resolved.key);
        }

        if let Some(description) = schema.description.as_deref() {
            state.add_metadata(description.len(), self.limits)?;
        }
        if schema.encrypted.is_some() {
            state.add_schema_bytes(/*bytes*/ 1, self.limits)?;
        }
        if schema.schema_type.is_some() {
            state.add_schema_bytes(/*bytes*/ 8, self.limits)?;
        }

        if let Some(enum_values) = schema.enum_values.as_ref() {
            state.enum_values = state.enum_values.saturating_add(enum_values.len());
            if state.enum_values > self.limits.max_schema_enum_values {
                return Err(EngineError::Limit(ArgumentRepairLimit::SchemaEnumValues));
            }
            for enum_value in enum_values {
                let bytes = bounded_json_size(enum_value, self.limits.max_schema_enum_bytes)
                    .ok_or(EngineError::Limit(ArgumentRepairLimit::SchemaEnumBytes))?;
                state.enum_bytes = state.enum_bytes.saturating_add(bytes);
                if state.enum_bytes > self.limits.max_schema_enum_bytes {
                    return Err(EngineError::Limit(ArgumentRepairLimit::SchemaEnumBytes));
                }
                state.add_schema_bytes(bytes.saturating_add(1), self.limits)?;
            }
        }

        if let Some(required) = schema.required.as_ref() {
            for name in required {
                state.add_name(name, self.limits)?;
            }
        }
        if let Some(properties) = schema.properties.as_ref() {
            for (name, property) in properties {
                state.add_name(name, self.limits)?;
                self.visit(property, depth + 1, state)?;
            }
        }
        if let Some(items) = schema.items.as_deref() {
            self.visit(items, depth + 1, state)?;
        }
        if let Some(AdditionalProperties::Schema(additional)) =
            schema.additional_properties.as_ref()
        {
            self.visit(additional, depth + 1, state)?;
        }
        for variants in [
            schema.any_of.as_ref(),
            schema.one_of.as_ref(),
            schema.all_of.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            for variant in variants {
                self.visit(variant, depth + 1, state)?;
            }
        }
        for definitions in [schema.defs.as_ref(), schema.definitions.as_ref()]
            .into_iter()
            .flatten()
        {
            for (name, definition) in definitions {
                state.add_name(name, self.limits)?;
                self.visit(definition, depth + 1, state)?;
            }
        }
        Ok(())
    }

    fn schema_at(&self, tokens: &[String]) -> Option<&'a JsonSchema> {
        let mut schema = self.root;
        let mut index = 0;
        while index < tokens.len() {
            match tokens[index].as_str() {
                "$defs" => {
                    let name = tokens.get(index + 1)?;
                    schema = schema.defs.as_ref()?.get(name)?;
                    index += 2;
                }
                "definitions" => {
                    let name = tokens.get(index + 1)?;
                    schema = schema.definitions.as_ref()?.get(name)?;
                    index += 2;
                }
                "properties" => {
                    let name = tokens.get(index + 1)?;
                    schema = schema.properties.as_ref()?.get(name)?;
                    index += 2;
                }
                "items" => {
                    schema = schema.items.as_deref()?;
                    index += 1;
                }
                "additionalProperties" => {
                    let AdditionalProperties::Schema(additional) =
                        schema.additional_properties.as_ref()?
                    else {
                        return None;
                    };
                    schema = additional;
                    index += 1;
                }
                "anyOf" => {
                    schema = indexed_schema(schema.any_of.as_ref()?, tokens.get(index + 1)?)?;
                    index += 2;
                }
                "oneOf" => {
                    schema = indexed_schema(schema.one_of.as_ref()?, tokens.get(index + 1)?)?;
                    index += 2;
                }
                "allOf" => {
                    schema = indexed_schema(schema.all_of.as_ref()?, tokens.get(index + 1)?)?;
                    index += 2;
                }
                _ => return None,
            }
        }
        Some(schema)
    }
}

#[derive(Default)]
struct TraversalState {
    nodes: usize,
    references: usize,
    schema_bytes: usize,
    metadata_bytes: usize,
    enum_values: usize,
    enum_bytes: usize,
    active_refs: BTreeSet<String>,
}

impl TraversalState {
    fn add_schema_bytes(
        &mut self,
        bytes: usize,
        limits: &ArgumentRepairLimits,
    ) -> Result<(), EngineError> {
        self.schema_bytes = self.schema_bytes.saturating_add(bytes);
        if self.schema_bytes > limits.max_schema_bytes {
            return Err(EngineError::Limit(ArgumentRepairLimit::SchemaBytes));
        }
        Ok(())
    }

    fn add_metadata(
        &mut self,
        bytes: usize,
        limits: &ArgumentRepairLimits,
    ) -> Result<(), EngineError> {
        self.metadata_bytes = self.metadata_bytes.saturating_add(bytes);
        if self.metadata_bytes > limits.max_schema_metadata_bytes {
            return Err(EngineError::Limit(ArgumentRepairLimit::SchemaMetadataBytes));
        }
        self.add_schema_bytes(bytes, limits)
    }

    fn add_name(&mut self, name: &str, limits: &ArgumentRepairLimits) -> Result<(), EngineError> {
        if name.len() > limits.max_schema_name_bytes {
            return Err(EngineError::Limit(ArgumentRepairLimit::SchemaNameBytes));
        }
        self.add_metadata(name.len(), limits)?;
        self.add_schema_bytes(/*bytes*/ 8, limits)
    }
}

fn bounded_json_size<T: Serialize>(value: &T, limit: usize) -> Option<usize> {
    let mut writer = BoundedCounter { bytes: 0, limit };
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.bytes)
}

struct BoundedCounter {
    bytes: usize,
    limit: usize,
}

impl io::Write for BoundedCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.bytes.saturating_add(bytes.len());
        if next > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bounded schema value exceeded limit",
            ));
        }
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn indexed_schema<'a>(schemas: &'a [JsonSchema], index: &str) -> Option<&'a JsonSchema> {
    schemas.get(index.parse::<usize>().ok()?)
}
