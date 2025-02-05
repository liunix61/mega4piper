use clap::Args;
use serde::{Deserialize, Serialize};

use crate::enums::ZtmType;

#[derive(Args, Clone, Debug)]
pub struct CommonOptions {
    #[arg(long, default_value_t = String::from("127.0.0.1"))]
    pub host: String,

    #[arg(long)]
    pub ztm: Option<ZtmType>,

    #[arg(long, default_value_t = 8001)]
    pub relay_port: u16,

    #[arg(long, default_value_t = 7777)]
    pub ztm_agent_port: u16,

    #[arg(long, default_value_t = 8888)]
    pub ztm_hub_port: u16,

    #[arg(long, default_value_t = 9999)]
    pub ca_port: u16,

    #[arg(long)]
    pub bootstrap_node: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GetParams {
    pub service: Option<String>,
    pub refspec: Option<String>,
    pub id: Option<String>,
    pub path: Option<String>,
    pub limit: Option<String>,
    pub cursor: Option<String>,
    pub identifier: Option<String>,
    pub port: Option<u16>,
}

#[derive(PartialEq, Eq, Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommonResult<T> {
    pub req_result: bool,
    pub data: Option<T>,
    pub err_message: String,
}

impl<T> CommonResult<T> {
    pub fn success(data: Option<T>) -> Self {
        CommonResult {
            req_result: true,
            data,
            err_message: "".to_owned(),
        }
    }
    pub fn failed(err_message: &str) -> Self {
        CommonResult {
            req_result: false,
            data: None,
            err_message: err_message.to_string(),
        }
    }
}
