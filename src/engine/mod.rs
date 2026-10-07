//! Поток движка apap-engine (§2.8, §4, ADR-01). Мост С3: `Player` живёт
//! внутри движка целиком, UI общается с ним через команды/события.

pub mod deps;
pub mod messages;
pub mod run;
pub mod sink;
pub mod source;
pub mod spawner;

#[cfg(test)]
mod tests;
