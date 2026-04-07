#!/usr/bin/env runme
//! [dependencies]
//! serde = "1"

use runme::prelude::*;

const __RUNME_GROUP: &str = "";

#[runme::task(desc = "Test task with deps")]
fn test_deps(_ctx: &TaskContext) {
    println!("Task with serde dependency");
}
