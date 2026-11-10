//! Service document and CSDL (`$metadata`) rendering.

use crate::{EdmType, EntitySet};
use serde_json::{json, Value};

const NAMESPACE: &str = "OpenBeam";

pub fn service_document(base_url: &str, sets: &[EntitySet]) -> Value {
    json!({
        "@odata.context": format!("{base_url}/$metadata"),
        "value": sets
            .iter()
            .map(|s| json!({ "name": s.name, "kind": "EntitySet", "url": s.name }))
            .collect::<Vec<_>>(),
    })
}

/// Names are engine-validated identifiers, so no XML escaping is needed.
pub fn metadata_xml(sets: &[EntitySet]) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <edmx:Edmx Version=\"4.0\" xmlns:edmx=\"http://docs.oasis-open.org/odata/ns/edmx\">\n\
         <edmx:DataServices>\n",
    );
    xml.push_str(&format!(
        "<Schema Namespace=\"{NAMESPACE}\" xmlns=\"http://docs.oasis-open.org/odata/ns/edm\">\n"
    ));
    for set in sets {
        xml.push_str(&format!("<EntityType Name=\"{}\">\n", set.name));
        xml.push_str(&format!("<Key><PropertyRef Name=\"{}\"/></Key>\n", set.key));
        for property in &set.properties {
            let (edm, facets) = match property.edm {
                EdmType::Boolean => ("Edm.Boolean", String::new()),
                EdmType::Int32 => ("Edm.Int32", String::new()),
                EdmType::Int64 => ("Edm.Int64", String::new()),
                EdmType::Double => ("Edm.Double", String::new()),
                EdmType::Decimal { precision, scale } => {
                    ("Edm.Decimal", format!(" Precision=\"{precision}\" Scale=\"{scale}\""))
                }
                EdmType::String => ("Edm.String", String::new()),
                EdmType::Date => ("Edm.Date", String::new()),
                EdmType::DateTimeOffset => ("Edm.DateTimeOffset", " Precision=\"6\"".to_string()),
            };
            let nullable = if property.name == set.key {
                " Nullable=\"false\""
            } else {
                ""
            };
            xml.push_str(&format!(
                "<Property Name=\"{}\" Type=\"{edm}\"{facets}{nullable}/>\n",
                property.name
            ));
        }
        xml.push_str("</EntityType>\n");
    }
    xml.push_str("<EntityContainer Name=\"Container\">\n");
    for set in sets {
        xml.push_str(&format!(
            "<EntitySet Name=\"{0}\" EntityType=\"{NAMESPACE}.{0}\"/>\n",
            set.name
        ));
    }
    xml.push_str("</EntityContainer>\n</Schema>\n</edmx:DataServices>\n</edmx:Edmx>\n");
    xml
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Property;

    #[test]
    fn metadata_declares_key_types_and_container() {
        let set = EntitySet {
            name: "sales".into(),
            key: "id".into(),
            properties: vec![
                Property {
                    name: "id".into(),
                    edm: EdmType::Int64,
                },
                Property {
                    name: "price".into(),
                    edm: EdmType::Decimal {
                        precision: 18,
                        scale: 2,
                    },
                },
            ],
        };
        let xml = metadata_xml(std::slice::from_ref(&set));
        assert!(xml.contains("<EntityType Name=\"sales\">"));
        assert!(xml.contains("<Key><PropertyRef Name=\"id\"/></Key>"));
        assert!(xml.contains("<Property Name=\"id\" Type=\"Edm.Int64\" Nullable=\"false\"/>"));
        assert!(xml.contains("<Property Name=\"price\" Type=\"Edm.Decimal\" Precision=\"18\" Scale=\"2\"/>"));
        assert!(xml.contains("<EntitySet Name=\"sales\" EntityType=\"OpenBeam.sales\"/>"));

        let doc = service_document("http://localhost:8787/odata", &[set]);
        assert_eq!(doc["value"][0]["url"], "sales");
        assert_eq!(doc["@odata.context"], "http://localhost:8787/odata/$metadata");
    }
}
