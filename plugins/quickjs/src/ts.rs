//! TypeScript support: types are erased before the code reaches QuickJS.

use swc_common::errors::Handler;
use swc_common::sync::Lrc;
use swc_common::{GLOBALS, Globals, SourceMap};

/// Strips TypeScript type annotations, leaving plain JavaScript. Types are
/// replaced with whitespace, so line and column numbers in errors still match
/// the original code. Plain JavaScript passes through unchanged.
pub fn strip_types(code: &str) -> Result<String, String> {
	let cm: Lrc<SourceMap> = Default::default();
	let handler = Handler::with_emitter_writer(Box::new(std::io::sink()), Some(cm.clone()));
	GLOBALS.set(&Globals::new(), || {
		swc_ts_fast_strip::operate(&cm, &handler, code.to_string(), Default::default())
			.map(|output| output.code)
			.map_err(|error| format!("invalid TypeScript: {}", error.message))
	})
}
