use crate::variable_definitions::{string_value, MutationAction, VariableSource};
use lazy_static::lazy_static;
use regex::{Captures, Regex};

use serde_yaml::{Mapping, Sequence, Value};

use std::fs::{read_to_string, File};

use std::cell::Cell;
use std::panic::PanicInfo;

use std::path::PathBuf;

// TODO support working in YAML but with Canonical JSON (RFC) output
#[derive(Debug)]
pub(crate) struct Environment {
    pub(crate) definitions: VariableSource,
    pub(crate) expected_runtime_lookup_prefixes: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum TemplateFormat {
    Yaml,
    Text,
}

#[derive(Debug)]
pub(crate) struct Template {
    pub(crate) format: TemplateFormat,
    pub(crate) source_path: PathBuf,
}

fn mapping_value(val: &mut Value) -> Option<&mut Mapping> {
    if let Value::Mapping(ref mut m) = val {
        return Some(m);
    }
    None
}
fn sequence_value(val: &mut Value) -> Option<&mut Sequence> {
    if let Value::Sequence(ref mut s) = val {
        return Some(s);
    }
    None
}

trait Navigate {
    fn navigate(&mut self, path: &[String]) -> &mut Value;
}
impl Navigate for Mapping {
    fn navigate(&mut self, path: &[String]) -> &mut Value {
        let next = self
            .get_mut(
                path.get(0)
                    .unwrap_or_else(|| panic!("WTF, regarding path {:?}", &path)),
            )
            .unwrap_or_else(|| panic!("WTF, regarding missing value at {:?}", &path));
        return next.navigate(&path[1..]);
    }
}
impl Navigate for Value {
    fn navigate(&mut self, path: &[String]) -> &mut Value {
        if path.is_empty() {
            return self;
        }
        mapping_value(self).expect("not a mapping").navigate(path)
    }
}

trait TryNavigate {
    fn try_navigate(&mut self, path: &[String]) -> Option<&mut Value>;
}
impl TryNavigate for Mapping {
    fn try_navigate(&mut self, path: &[String]) -> Option<&mut Value> {
        let next = self.get_mut(
            path.get(0)
                .unwrap_or_else(|| panic!("WTF, regarding path {:?}", &path)),
        );
        next.and_then(|next| next.try_navigate(&path[1..]))
    }
}
impl TryNavigate for Value {
    fn try_navigate(&mut self, path: &[String]) -> Option<&mut Value> {
        if path.is_empty() {
            return Some(self);
        }
        mapping_value(self)
            .expect("not a mapping")
            .try_navigate(path)
    }
}

fn apply_mutation(mutation: &MutationAction, content: &mut Value) {
    match mutation {
        MutationAction::Add(path, Value::Mapping(new_entries)) => {
            let current = mapping_value(content.navigate(path)).expect("urm");
            for (k, v) in new_entries.iter() {
                let old_val = current.insert(k.clone(), v.clone());
                if old_val.is_some() {
                    panic!("Already had value at {path:?}")
                }
            }
        }
        MutationAction::Add(path, Value::Sequence(new_elems)) => {
            let current = sequence_value(content.navigate(path)).expect("urm");
            for v in new_elems.iter() {
                current.push(v.clone());
            }
        }
        MutationAction::Add(_path, _) => {
            panic!("Add mutation is trying to add non-mapping, non-sequence values")
        }
        MutationAction::Remove(path) => {
            mapping_value(content.navigate(&path[..(path.len() - 1)]))
                .expect("not a mapping")
                .remove(&path[path.len() - 1])
                .unwrap_or_else(|| panic!("can't remove missing {:?}", &path));
        }
        MutationAction::Replace(path, v) => {
            let current =
                mapping_value(content.navigate(&path[..(path.len() - 1)])).expect("not a mapping");
            let old_val =
                current.insert(Value::String(path[path.len() - 1].to_string()), v.clone());
            if old_val.is_none() {
                panic!("Value to replace at {:?} did not exist", &path)
            }
        }
    }
}

fn _lookup(reference_name: &str, environment: &Environment) -> Option<Value> {
    let maybe = environment.definitions.definitions.get(reference_name);
    match maybe {
        None => {
            let last_slash = reference_name[..reference_name.len() - 2].rfind('/');
            match last_slash {
                None => None,
                Some(split_pos) => _lookup(
                    &(reference_name[..split_pos].to_string() + "/*"),
                    environment,
                ),
            }
        }
        Some(value) => Some(value.clone()),
    }
}
fn lookup(reference_name: &str, environment: &Environment) -> Option<Value> {
    let should_be_runtime_value = environment
        .expected_runtime_lookup_prefixes
        .iter()
        .any(|prefix| reference_name.starts_with(prefix));
    let should_be_json = reference_name.ends_with("/json");

    match _lookup(reference_name, environment) {
        None => {
            if should_be_runtime_value {
                None
            } else {
                panic!("Couldn't find definition for {}", &reference_name)
            }
        }
        Some(val) => {
            if should_be_runtime_value {
                eprintln!(
                    "WARN: Runtime value \"{reference_name}\" was unexpectedly hardcoded."
                )
            }
            if should_be_json {
                let expanded_val = expand(val, environment);
                if let Value::Mapping(m) = expanded_val {
                    Some(Value::String(
                        json_canon::to_string(&serde_json::to_value(m).unwrap()).unwrap(),
                    ))
                } else if let Value::String(s) = expanded_val {
                    Some(Value::String(s))
                } else {
                    panic!(
                        "Received non-mapping value for /json conversion: {:?}",
                        &expanded_val
                    )
                }
            } else {
                Some(expand(val, environment))
            }
        }
    }
}

lazy_static! {
    static ref VAR_SUBSTITUTION_PATTERN: Regex = Regex::new(r"\(\(\s*([^) ]*?)\s*\)\)").unwrap();
    static ref FULL_MATCH_PATTERN: Regex =
        Regex::new(r"\A\s*\(\(\s*([^) ]*?)\s*\)\)\s*\z").unwrap();
}

fn expand_string(string: String, environment: &Environment) -> Value {
    if let Some(captures) = FULL_MATCH_PATTERN.captures(&string) {
        let ref_name = captures.get(1).unwrap().as_str();
        return lookup(ref_name, environment).unwrap_or(Value::String(string));
    }
    let substituted = VAR_SUBSTITUTION_PATTERN.replace_all(&string, |captures: &Captures| {
        let ref_name = captures.get(1).unwrap().as_str();
        let val = lookup(ref_name, environment);
        match val {
            None => format!("(( {ref_name} ))"),
            Some(Value::Number(n)) => format!("{n}"),
            Some(Value::String(str)) => str,
            Some(_) => panic!(
                "Attempted to interpolate non-string value \"{ref_name}\" ({val:?})"
            ),
        }
    });
    Value::String(substituted.to_string())
}

fn expand(content: Value, environment: &Environment) -> Value {
    match content {
        Value::Null => Value::Null,
        Value::Bool(a) => Value::Bool(a),
        Value::Number(a) => Value::Number(a),
        Value::String(str) => expand_string(str, environment),
        Value::Sequence(seq) => {
            Value::Sequence(seq.into_iter().map(|v| expand(v, environment)).collect())
        }
        Value::Mapping(map) => {
            let mut stuff = map
                .into_iter()
                .map(|(k, v)| (expand(k, environment), v))
                .collect::<Vec<_>>();
            stuff.sort_by_key(|(k, _v)| string_value(k));
            let stuff = stuff
                .into_iter()
                .map(|(k, v)| (k, expand(v, environment)))
                .collect();
            Value::Mapping(stuff)
        }
        Value::Tagged(_) => {
            panic!("what the fuck is this?")
        }
    }
}

thread_local! {
    static CURRENT_FILE: Cell<Option<String>> = const { Cell::new(None) };
    static DEFAULT_HOOK: Cell<Option<Box<dyn Fn(&PanicInfo)->()>>> = const { Cell::new(None) };
}

fn panic_hook(info: &PanicInfo) {
    CURRENT_FILE.with(|f| {
        if let Some(f) = f.take().as_ref() {
            eprintln!("\nFailed to compile \"{}\"", &f);
        };
    });
    DEFAULT_HOOK.with(|def_hook| {
        def_hook.take().map(|f| f(info) );
    });
}

fn with_error_catcher<T>(output_path: String, processor: &dyn Fn()->T) -> T {
    CURRENT_FILE.with(|f| {
        f.set(Some(output_path));
    });
    DEFAULT_HOOK.with(|def_hook| {
        def_hook.set(Some(std::panic::take_hook()));
        std::panic::set_hook(Box::new(panic_hook));
    });
    let content = processor();
    let _ = std::panic::take_hook();
    CURRENT_FILE.with(|f| {
        f.set(None);
    });
    content
}

pub(crate) fn process_text(template: &Template, environment: &Environment, output_path: String) -> String {
    with_error_catcher(output_path, &|| {
        let text = read_to_string(&template.source_path).unwrap();
        string_value(&expand_string(text, environment))
            .expect("Text template somehow expanded to a non-string value")
    })
}

pub(crate) fn process_yaml(template: &Template, environment: &Environment, output_path: String) -> Value {
    with_error_catcher(output_path, &|| {
        let filename = template.source_path.file_name().unwrap().to_str().unwrap();
        let mut content: Value =
            serde_yaml::from_reader(File::open(&template.source_path).unwrap()).unwrap();

        for mutation in &environment.definitions.mutations {
            if mutation.filename_pattern == filename {
                apply_mutation(&mutation.action, &mut content);
            }
        }
        let mut content = expand(content, environment);
        postprocess_yaml(&mut content);
        content
    })
}

fn postprocess_yaml(_yaml_config: &mut Value) {
    // i've left this here as an example of doing this kind of thing
    // it can be nice to work around frameworks which have an annoying config format
    // (unless what's annoying is that they're incompatible with json, obviously)

    // however i'm commenting it out as this thing is supposed to have pure behaviour by default. fork it and build a different version for custom behaviour :)

    // if let Some(profiles) = yaml_config.try_navigate(&vec!["spring".to_string(), "profiles".to_string()]) {
    //     if let Value::Mapping(profiles) = profiles {
    //         if let Some(Value::Sequence(active_profiles)) = profiles.get("active") {
    //             profiles.insert(
    //                 Value::String("active".to_string()),
    //                 Value::String(
    //                     active_profiles.iter().map(|prof| {
    //                         string_value(prof).unwrap()
    //                     }).collect::<Vec<_>>().join(", ")
    //                 )
    //             );
    //         }
    //     }
    // }
}
