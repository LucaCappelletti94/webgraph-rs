#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use webgraph::prelude::*;

#[derive(Arbitrary, Debug)]
struct FuzzCase {
    init: Vec<u64>,
    commands: Vec<RandomCommand>
}

#[derive(Arbitrary, Debug)]
enum RandomCommand {
    Len,
    GetPosition,
    SetPosition(usize),
    ReadNextWord,
    WriteWord(u64),
}

fuzz_target!(|data: FuzzCase| {
    let mut idx = 0;
    let mut buffer = data.init.clone();
    let mut buffer2 = data.init.clone();

    let mut writer = MemWordWrite::new(&mut buffer2);
    for command in data.commands {
        match command {
            RandomCommand::Len => {
                assert_eq!(writer.len(), buffer.len());
            },
            RandomCommand::GetPosition => {
                assert_eq!(writer.get_position(), idx);
            },
            RandomCommand::SetPosition(word_index) => {
                let _ = writer.set_position(word_index);
                if buffer.get(word_index).is_some() {
                    idx = word_index;
                }
            },
            RandomCommand::ReadNextWord => {
                assert_eq!(writer.read_next_word().ok(), buffer.get(idx).copied());
                if buffer.get(idx).is_some() {
                    idx += 1;
                }
            },
            RandomCommand::WriteWord(word) => {
                let can_write = if let Some(w) = buffer.get_mut(idx) {
                    *w = word;
                    true
                } else {
                    false
                };
                assert_eq!(writer.write_word(word).is_ok(), can_write);
                if can_write {
                    idx += 1;
                }
            },
        };
    }
});
