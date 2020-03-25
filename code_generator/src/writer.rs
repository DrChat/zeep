use inflector::cases::pascalcase::to_pascal_case;
use inflector::cases::snakecase::to_snake_case;
use log::{info, warn};
use roxmltree::Node;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::{stdout, Cursor, Read, Write};
use std::mem::discriminant;

const MESSAGES_MOD: &str = "messages";
const TYPES_MOD: &str = "types";
const PORTS_MOD: &str = "ports";
const BINDINGS_MOD: &str = "bindings";
const SOAP_ENV: &str = "soapenv";

pub struct FileWriter {
    base_path: String,
    current_section: Section,
    mod_writers: HashMap<Section, ModWriter>,
    level: usize,
    writer: Option<Box<dyn std::io::Write>>,
    target_name_space: Option<String>,
}

struct ModWriter {
    level: usize,
    section: Section,
    buffers: Vec<Cursor<Vec<u8>>>,
    delayed_buffer: Cursor<Vec<u8>>,
    final_stage: Cursor<Vec<u8>>,
    defined_types: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum Section {
    Root,
    Types,
    Messages,
    PortTypes,
    Bindings,
}

impl Default for FileWriter {
    fn default() -> Self {
        FileWriter {
            base_path: String::default(),
            current_section: Section::Root,
            mod_writers: FileWriter::init_mod_writers(),
            level: 0,
            writer: Option::Some(Box::new(stdout())),
            target_name_space: Option::None,
        }
    }
}

impl FileWriter {
    pub fn new_file(dest_file_name: File) -> Self {
        FileWriter {
            base_path: String::default(),
            current_section: Section::Root,
            mod_writers: FileWriter::init_mod_writers(),
            level: 0,
            writer: Option::Some(Box::new(dest_file_name)),
            target_name_space: Option::None,
        }
    }

    fn init_mod_writers() -> HashMap<Section, ModWriter> {
        let mut mod_writers = HashMap::new();
        mod_writers.insert(Section::Root, ModWriter::new(Section::Root));
        mod_writers.insert(Section::Messages, ModWriter::new(Section::Messages));
        mod_writers.insert(Section::Types, ModWriter::new(Section::Types));
        mod_writers.insert(Section::PortTypes, ModWriter::new(Section::PortTypes));
        mod_writers.insert(Section::Bindings, ModWriter::new(Section::Bindings));
        mod_writers
    }

    pub fn process_file(&mut self, base_path: &str, file_name: &str) {
        self.base_path = base_path.to_string();
        self.print_header();
        self.process_file_in_path(file_name, true);
    }

    fn process_file_in_path(&mut self, file_name: &str, print_when_done: bool) {
        let f_in = format!("{}/{}", self.base_path, file_name);
        let xml = std::fs::read_to_string(f_in).expect("can not read file");
        let doc = roxmltree::Document::parse(&xml).unwrap();
        doc.descendants().for_each(|n| self.print(&n));

        if !print_when_done {
            return;
        }

        // once all elements are processed, write them to output
        for (_section, mw) in self.mod_writers.iter_mut() {
            let reader_ref = mw.read_for_output();
            let mut reader = reader_ref.into_inner();

            if let Some(mut writer) = self.writer.take() {
                if let Err(err) = io::copy(&mut reader, &mut writer) {
                    warn!("Failed to flush final stage to output: {:?}", err);
                }

                // return writer for next loop
                self.writer = Option::Some(writer);
            }
        }
    }

    fn write(&mut self, buf: String) {
        if let Some(mw) = self.mod_writers.get_mut(&self.current_section) {
            mw.write(buf, self.level)
        }
    }

    fn delayed_write(&mut self, buf: String) {
        if let Some(mw) = self.mod_writers.get_mut(&self.current_section) {
            mw.delayed_write(buf)
        }
    }

    fn flush_delayed_buffer(&mut self) {
        if let Some(mw) = self.mod_writers.get_mut(&self.current_section) {
            mw.flush_delayed_buffer()
        }
    }

    pub fn seen_type(&mut self, type_def: String) {
        if let Some(mw) = self.mod_writers.get_mut(&self.current_section) {
            mw.seen_type(type_def);
        }
    }

    pub fn reset_defined_types(&mut self) {
        if let Some(mw) = self.mod_writers.get_mut(&self.current_section) {
            mw.reset_defined_types();
        }
    }

    pub fn have_seen_type(&mut self, type_def: String) -> bool {
        if let Some(mw) = self.mod_writers.get_mut(&self.current_section) {
            mw.have_seen_type(type_def)
        } else {
            false
        }
    }

    fn set_level(&mut self, level: usize) {
        self.level = level;
        if let Some(mw) = self.mod_writers.get_mut(&self.current_section) {
            mw.set_level(level);
        }
    }

    fn inc_level(&mut self) {
        self.set_level(self.level + 1);
    }

    fn dec_level(&mut self) {
        self.set_level(self.level - 1);
    }

    fn print_header(&mut self) {
        self.write(
            r#"use yaserde::{{YaSerialize, YaDeserialize}};
            use std::io::{Read, Write};
            use soap_client::soap::Header;
            use soap_client::envelop;
            
            "#
            .to_string(),
        );
    }

    fn print(&mut self, node: &Node) {
        if !node.is_element() {
            return;
        }

        match node.tag_name().name() {
            "definitions" => self.print_definitions(node),
            "schema" => self.print_xsd(node),
            _ => {}
        }
    }

    fn print_definitions(&mut self, node: &Node) {
        node.children()
            .for_each(|child| match child.tag_name().name() {
                "types" => self.print_xsd(&child),
                "message" => self.print_message(&child),
                "portType" => self.print_port_type(&child),
                "binding" => self.print_binding(&child),
                _ => {}
            })
    }

    fn print_xsd(&mut self, node: &Node) {
        self.check_section(Section::Types);

        self.target_name_space = self
            .get_some_attribute(node, "targetNamespace")
            .map(|s| s.to_string());

        node.children()
            .for_each(|child| match child.tag_name().name() {
                "import" => self.import_file(&child),
                "element" => self.print_element(&child),
                "complexType" => {
                    if let Some(n) = self.get_some_attribute(&child, "name") {
                        self.print_complex_element(&child, n)
                    };
                }
                _ => {}
            })
    }

    fn import_file(&mut self, node: &Node) {
        let name = match self.get_some_attribute(node, "schemaLocation") {
            None => return,
            Some(n) => n,
        };

        self.process_file_in_path(name, false);
    }

    fn print_element(&mut self, node: &Node) {
        let name = match self.get_some_attribute(node, "name") {
            None => return,
            Some(n) => n,
        };

        let as_vec = self.get_some_attribute(node, "maxOccurs").is_some();
        let as_option = self.get_some_attribute(node, "nillable").is_some();

        let maybe_complex = node
            .children()
            .find(|child| child.has_tag_name("complexType"));

        // fields
        if let Some(complex) = maybe_complex {
            self.print_complex_element(&complex, name)
        } else if let Some(element_name) = self.get_some_attribute(node, "name") {
            if let Some(type_name) = self.get_some_attribute(node, "type") {
                if self.level == 0 {
                    // top-level == type alias
                    self.write(format!(
                        "pub type {} = {};\n\n",
                        to_pascal_case(element_name),
                        self.fetch_type(type_name)
                    ));
                    return;
                }

                if let Some(tns) = &self.target_name_space {
                    self.write(format!(
                        "\t#[yaserde(prefix = \"ns\", rename = \"{}\", default)]\n",
                        element_name,
                    ));
                } else {
                    self.write(format!(
                        "\t#[yaserde(rename = \"{}\", default)]\n",
                        element_name,
                    ));
                }

                if as_vec || as_option {
                    self.write(format!(
                        "\tpub {}: {}<{}>,\n",
                        self.shield_reserved_names(&to_snake_case(element_name)),
                        if as_vec { "Vec" } else { "Option" },
                        self.fetch_type(type_name)
                    ));
                } else {
                    self.write(format!(
                        "\tpub {}: {},\n",
                        self.shield_reserved_names(&to_snake_case(element_name)),
                        self.fetch_type(type_name)
                    ));
                }
            }
        }
    }

    fn get_some_attribute<'a>(&self, node: &'a Node, attr_name: &str) -> Option<&'a str> {
        match node.attributes().iter().find(|a| a.name() == attr_name) {
            None => None,
            Some(a) => Some(a.value()),
        }
    }

    fn get_some_attribute_as_string(&self, node: &Node, attr_name: &str) -> Option<String> {
        match node.attributes().iter().find(|a| a.name() == attr_name) {
            None => None,
            Some(a) => Some(a.value().to_string()),
        }
    }

    fn fetch_type(&self, node_type: &str) -> String {
        match self.split_type(node_type) {
            "string" | "base64Binary" => "String".to_string(),
            "decimal" => "f64".to_string(),
            "integer" | "int" | "long" => "u64".to_string(),
            "short" => "u8".to_string(),
            "boolean" => "bool".to_string(),
            "date" | "xs:time" => "SystemTime".to_string(),
            v => to_pascal_case(v),
        }
    }

    fn split_type<'a>(&self, node_type: &'a str) -> &'a str {
        match node_type.split(':').last() {
            None => "String",
            Some(v) => v,
        }
    }

    fn print_complex_element(&mut self, node: &Node, name: &str) {
        self.inc_level();
        self.write("#[derive(Debug, Default, YaSerialize, YaDeserialize)]\n".to_string());

        if let Some(tns) = &self.target_name_space {
            self.write(format!(
                "#[yaserde(prefix = \"ns\", namespace = \"ns: {}\", rename = \"{}\", default)]\npub struct {} {{\n",
                tns,
                name,
                to_pascal_case(name)
            ));
        } else {
            self.write(format!(
                "#[yaserde(rename = \"{}\", default)]\npub struct {} {{\n",
                name,
                to_pascal_case(name)
            ));
        }

        let maybe_sequence = node.children().find(|child| child.has_tag_name("sequence"));

        let maybe_complex = node
            .children()
            .find(|child| child.has_tag_name("complexContent"));

        if let Some(sequence) = maybe_sequence {
            self.print_sequence(&sequence);
        }

        if let Some(complex) = maybe_complex {
            self.print_complex_content(&complex);
        }

        self.write("}\n\n".to_string());
        self.dec_level();
    }

    fn print_sequence(&mut self, node: &Node) {
        node.children().for_each(|child| self.print_element(&child));
    }

    fn print_complex_content(&mut self, node: &Node) {
        if let Some(extension) = node
            .children()
            .find(|child| child.has_tag_name("extension"))
        {
            self.write("\t#[yaserde(flatten)]\n".to_string());
            self.print_extension(&extension);

            let maybe_sequence = extension
                .children()
                .find(|ext_child| ext_child.has_tag_name("sequence"));

            if let Some(sequence) = maybe_sequence {
                self.print_sequence(&sequence);
            }
        }

        self.print_sequence(node);
    }

    fn print_extension(&mut self, node: &Node) {
        let base = match self.get_some_attribute(node, "base") {
            None => return,
            Some(n) => n,
        };

        self.write(format!(
            "\tpub {}: {},\n",
            to_snake_case(&self.fetch_type(base)),
            self.fetch_type(base)
        ));
    }

    fn shield_reserved_names<'a>(&self, type_name: &'a str) -> &'a str {
        match type_name {
            "type" => "rs_type",
            other => other,
        }
    }

    fn check_section(&mut self, target: Section) {
        if discriminant(&self.current_section) != discriminant(&target) {
            self.current_section = target;
        }
    }

    // WSDL Messages

    fn print_message(&mut self, node: &Node) {
        self.check_section(Section::Messages);

        if let Some(name) = self.get_some_attribute(node, "name") {
            self.write("#[derive(Debug, Default, YaSerialize, YaDeserialize)]\n".to_string());

            self.write(format!(
                "#[yaserde(rename = \"{}\", default)]\npub struct {} {{\n",
                name,
                to_pascal_case(name)
            ));

            let maybe_part = node.children().find(|child| child.has_tag_name("part"));

            if let Some(part) = maybe_part {
                self.print_part(&part);
            }

            self.write("}\n\n".to_string());
        }
    }

    fn print_part(&mut self, node: &Node) {
        let element_name = match self.get_some_attribute(node, "name") {
            None => return,
            Some(n) => n,
        };

        if let Some(type_name) = self.get_some_attribute(node, "element") {
            self.write("\t#[yaserde(flatten)]\n".to_string());

            self.write(format!(
                "\tpub {}: {}::{},\n",
                self.shield_reserved_names(&to_snake_case(element_name)),
                TYPES_MOD,
                self.fetch_type(type_name)
            ));
        }
    }

    // WSDL Port Types
    fn print_port_type(&mut self, node: &Node) {
        self.check_section(Section::PortTypes);
        let element_name = match self.get_some_attribute(node, "name") {
            None => return,
            Some(n) => n,
        };

        let struct_name = to_pascal_case(element_name);
        self.write(format!("pub trait {0} {{\n", struct_name));
        node.children()
            .for_each(|child| self.print_operation(&child));
        self.write("}\n\n".to_string());
        self.flush_delayed_buffer();
        self.reset_defined_types();
    }

    // WSDL bindings

    fn print_binding(&mut self, node: &Node) {
        self.check_section(Section::Bindings);
        let element_name = match self.get_some_attribute(node, "name") {
            None => return,
            Some(n) => n,
        };

        let type_name = match self.get_some_attribute(node, "type") {
            None => return,
            Some(n) => n,
        };

        let struct_name = to_pascal_case(element_name);
        let trait_name = self.fetch_type(type_name);

        self.write(format!(
            "pub struct {0} {{}}\n\nimpl {2}::{1} for {0} {{\n",
            struct_name, trait_name, PORTS_MOD,
        ));

        node.children()
            .for_each(|child| self.print_binding_operation(&child));
        self.write("}\n\n".to_string());
        self.print_default_constructor(struct_name);
        self.flush_delayed_buffer();
    }

    fn print_default_constructor(&mut self, struct_name: String) {
        self.write(format!(
            "impl Default for {0} {{\n\tfn default() -> Self {{\n\t\t{0}{{}}\n\t}}\n\t}}\n",
            struct_name
        ));
    }

    fn map_name_message(&self, node: &Node) -> (Option<String>, Option<String>) {
        (
            self.get_some_attribute_as_string(node, "name"),
            self.get_some_attribute_as_string(node, "message"),
        )
    }

    fn print_operation(&mut self, node: &Node) {
        let element_name = match self.get_some_attribute(node, "name") {
            None => return,
            Some(n) => n,
        };

        let func_name = to_snake_case(element_name);
        let some_input = node
            .children()
            .find(|c| c.has_tag_name("input"))
            .map(|c| self.map_name_message(&c));

        let some_output = node
            .children()
            .find(|c| c.has_tag_name("output"))
            .map(|c| self.map_name_message(&c));

        let some_fault = node
            .children()
            .find(|c| c.has_tag_name("fault"))
            .map(|c| self.map_name_message(&c));

        let (input_type_template, input_template) = match some_input {
            Some((Some(name), Some(msg))) => (
                format!(
                    "pub type {0} = {1}::{2};\n",
                    to_pascal_case(name.as_str()),
                    MESSAGES_MOD,
                    self.fetch_type(msg.as_str())
                ),
                format!(
                    "{}: {}",
                    to_snake_case(name.as_str()),
                    to_pascal_case(name.as_str())
                ),
            ),
            _ => ("".to_string(), "".to_string()),
        };

        let (output_type_template, fault_type_template, output_template) = match some_output {
            Some((Some(name), Some(msg))) => {
                if let Some((Some(fault_name), Some(fault_type))) = some_fault {
                    (
                        format!(
                            "pub type {} = {}::{};\n",
                            to_pascal_case(name.as_str()),
                            MESSAGES_MOD,
                            self.fetch_type(msg.as_str())
                        ),
                        Option::Some(format!(
                            "pub type {} = {}::{};\n",
                            to_pascal_case(fault_name.as_str()),
                            MESSAGES_MOD,
                            self.fetch_type(fault_type.as_str())
                        )),
                        format!(
                            "-> Result<{0}, {1}>",
                            to_pascal_case(name.as_str()),
                            to_pascal_case(fault_name.as_str())
                        ),
                    )
                } else {
                    (
                        format!(
                            "pub type {} = {}::{};\n",
                            to_pascal_case(name.as_str()),
                            MESSAGES_MOD,
                            self.fetch_type(msg.as_str())
                        ),
                        Option::None,
                        format!("-> {}", to_pascal_case(name.as_str())),
                    )
                }
            }
            _ => ("".to_string(), Option::None, "".to_string()),
        };

        self.queue_port_types(
            &input_type_template,
            &output_type_template,
            fault_type_template,
        );

        self.write(format!(
            "\tfn {} (&self, {}) {};\n",
            func_name, input_template, output_template,
        ));
    }

    fn queue_port_types(&mut self, input: &str, output: &str, fault: Option<String>) {
        // make sure these messages get written at the end of the module

        if !self.have_seen_type(input.to_string()) {
            self.delayed_write(input.to_string());
            self.seen_type(input.to_string());
        }

        if !self.have_seen_type(output.to_string()) {
            self.delayed_write(output.to_string());
            self.seen_type(output.to_string());
        }

        if let Some(f) = fault {
            if !self.have_seen_type(f.to_string()) {
                self.seen_type(f.to_string());
                self.delayed_write(f);
            }
        }
    }

    fn construct_soap_wrapper(&self, soap_name: &str, body_type: &str) -> String {
        format!(
            r#"#[derive(Debug, Default, YaSerialize, YaDeserialize)]
        #[yaserde(
            root = "Envelope",
            namespace = "soapenv: http://schemas.xmlsoap.org/soap/envelope/",
            prefix = "soapenv"
        )]
        pub struct {0}SoapEnvelope {{
            #[yaserde(rename = "encodingStyle", prefix = "soapenv", attribute)]
            pub encoding_style: String,
            #[yaserde(rename = "tns", prefix = "xmlns", attribute)]
            pub tnsattr: String,
            #[yaserde(rename = "urn", prefix = "xmlns", attribute)]
            pub urnattr: Option<String>,
            #[yaserde(rename = "xsi", prefix = "xmlns", attribute)]
            pub xsiattr: String,
            #[yaserde(rename = "Header", prefix = "soapenv")]
            pub header: Option<Header>,
            #[yaserde(rename = "Body", prefix = "soapenv")]
            pub body: {1},
        }}
        "#,
            soap_name, body_type
        )
    }

    fn print_binding_operation(&mut self, node: &Node) {
        let element_name = match self.get_some_attribute(node, "name") {
            None => return,
            Some(n) => n,
        };

        let func_name = to_snake_case(element_name);
        let some_input = node
            .children()
            .find(|c| c.has_tag_name("input"))
            .map(|c| self.get_some_attribute_as_string(&c, "name"));

        let some_output = node
            .children()
            .find(|c| c.has_tag_name("output"))
            .map(|c| self.get_some_attribute_as_string(&c, "name"));

        let some_fault = node
            .children()
            .find(|c| c.has_tag_name("fault"))
            .map(|c| self.get_some_attribute_as_string(&c, "name"));

        let (input_template, soap_wrapper_in) = match some_input {
            Some(Some(name)) => {
                let pascal_name = to_pascal_case(name.as_str());
                let soap_name = format!("Soap{}", pascal_name);

                (format!(
                    "{}: {}::{}",
                    to_snake_case(name.as_str()),
                    PORTS_MOD,
                    pascal_name
                ),
                format!(
                    "#[derive(Debug, Default, YaSerialize, YaDeserialize)]\npub struct {0} {{\n\t#[yaserde(rename = \"{3}\", default)]\n\tpub body: {2}::{1},\n}}\n{4}\n",
                    soap_name,
                    to_pascal_case(name.as_str()),
                    PORTS_MOD,
                    element_name,
                    self.construct_soap_wrapper(pascal_name.as_str(), soap_name.as_str())
                ))
            }
            _ => ("".to_string(), "".to_string()),
        };

        let (output_template, soap_wrapper_out) = match some_output {
            Some(Some(name)) => {
                if let Some(Some(fault_name)) = some_fault {
                    let pascal_name = to_pascal_case(name.as_str());
                    let pascal_fault_name = to_pascal_case(fault_name.as_str());
                    let soap_name = format!("Soap{}", pascal_name);

                    (format!(
                        "-> Result<{2}::{0}, {2}::{1}>",
                        pascal_name,
                        pascal_fault_name,
                        PORTS_MOD,
                    ),
                    format!(
                        "#[derive(Debug, Default, YaSerialize, YaDeserialize)]\npub struct {0} {{\n\t#[yaserde(rename = \"{3}\", default)]\n\tpub body: {2}::{1},\n}}\n{4}\n",
                        soap_name,
                        pascal_name,
                        PORTS_MOD,
                        element_name,
                        self.construct_soap_wrapper(pascal_name.as_str(), soap_name.as_str())
                    ))
                } else {
                    let pascal_name = to_pascal_case(name.as_str());
                    let soap_name = format!("Soap{}", pascal_name);
                    (format!("-> {}::{}", PORTS_MOD, pascal_name),
                    format!(
                        "#[derive(Debug, Default, YaSerialize, YaDeserialize)]\npub struct {0} {{\n\t#[yaserde(rename = \"{3}\", default)]\n\tpub body: {2}::{1},\n}}\n{4}\n",
                        soap_name,
                        pascal_name,
                        PORTS_MOD,
                        element_name,
                        self.construct_soap_wrapper(pascal_name.as_str(), soap_name.as_str())
                    ))
                }
            }
            _ => ("".to_string(), "".to_string()),
        };

        self.write(format!(
            "\tfn {} (&self, {}) {} {{\n",
            func_name, input_template, output_template,
        ));
        self.write("\tunimplemented!();\n".to_string());
        self.write("}\n".to_string());
        self.delayed_write(soap_wrapper_in);
        self.delayed_write(soap_wrapper_out);
    }
}

impl ModWriter {
    fn new(section: Section) -> Self {
        let mut mw = ModWriter {
            section,
            buffers: vec![],
            delayed_buffer: Cursor::new(vec![]),
            final_stage: Cursor::new(vec![]),
            level: 0,
            defined_types: vec![],
        };

        match &mw.section {
            Section::Root => {}
            Section::Types => mw.print_mod_header(TYPES_MOD),
            Section::Messages => mw.print_mod_header(MESSAGES_MOD),
            Section::PortTypes => mw.print_mod_header(PORTS_MOD),
            Section::Bindings => mw.print_mod_header(BINDINGS_MOD),
        }

        mw
    }

    fn print_mod_header(&mut self, mod_name: &str) {
        self.write(format!("pub mod {} {{\n", mod_name), 0);
        self.print_header();
        self.write("use super::*;\n\n".to_string(), 0);
    }

    fn print_header(&mut self) {
        self.write(
            "use yaserde::{{YaSerialize, YaDeserialize}};\n\n".to_string(),
            0,
        );
    }

    fn print_footer(&mut self) {
        if let Section::Root = self.section {
        } else {
            self.write("}\n\n".to_string(), 0);
        }
    }

    fn flush_buffers(&mut self) {
        while let Some(mut cursor) = self.buffers.pop() {
            cursor.set_position(0);
            if let Err(err) = io::copy(&mut cursor, &mut self.final_stage) {
                warn!("Failed to flush buffer: {:?}", err);
            }
        }
    }

    fn flush_delayed_buffer(&mut self) {
        self.delayed_buffer.set_position(0);
        if self.level == 0 {
            if let Err(err) = io::copy(&mut self.delayed_buffer, &mut self.final_stage) {
                warn!("Failed to flush buffer: {:?}", err);
            }
        } else if let Some(mut buffer) = self.buffers.get_mut(self.level - 1) {
            if let Err(err) = io::copy(&mut self.delayed_buffer, &mut buffer) {
                warn!("Failed to flush buffer: {:?}", err);
            }
        }
    }

    fn set_level(&mut self, level: usize) {
        self.level = level;

        if level == 0 {
            self.flush_buffers();
        }

        if self.buffers.len() < (self.level as usize) {
            let b = Cursor::new(Vec::new());
            self.buffers.push(b);
        }
    }

    pub fn write(&mut self, buf: String, level: usize) {
        if level == 0 {
            // write to output
            self.final_stage
                .write_all(buf.as_bytes())
                .expect("can not write to output");
        } else if let Some(buffer) = self.buffers.get_mut(level - 1) {
            // store in buffer
            buffer
                .write_all(buf.as_bytes())
                .expect("can not write buffer");
        }
    }

    pub fn delayed_write(&mut self, buf: String) {
        self.delayed_buffer
            .write_all(buf.as_bytes())
            .expect("can not write to delayed buffer");
    }

    pub fn read_for_output(&mut self) -> RefCell<impl Read> {
        self.print_footer();
        self.final_stage.set_position(0);
        RefCell::new(self.final_stage.clone())
    }

    pub fn seen_type(&mut self, type_def: String) {
        self.defined_types.push(type_def);
    }

    pub fn reset_defined_types(&mut self) {
        self.defined_types.clear();
    }

    pub fn have_seen_type(&self, type_def: String) -> bool {
        self.defined_types.contains(&type_def)
    }
}
