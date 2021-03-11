use std::any::Any;

use fxhash::FxHashMap;
use linkme::distributed_slice;
use smallbox::SmallBox;

pub type LocalTypeId = u32;

pub trait Message: Any + Send {
    #[doc(hidden)]
    const _LTID: LocalTypeId;
}

pub trait Request: Message {
    type Response: Message;
}

pub type AnyMessage = SmallBox<dyn Any + Send, [u8; 80]>;

#[derive(Clone)]
pub struct MessageVTable {
    pub ltid: LocalTypeId,
    pub clone: fn(&AnyMessage) -> AnyMessage,
}

#[distributed_slice]
pub static MESSAGE_LIST: [MessageVTable] = [..];

thread_local! {
    // TODO: access it speculatively during initialization.
    static MESSAGE_BY_LTID: FxHashMap<LocalTypeId, MessageVTable> = {
        MESSAGE_LIST.iter()
            .map(|vtable| (vtable.ltid, vtable.clone()))
            .collect()
    };
}

pub(crate) fn with_vtable<R>(ltid: LocalTypeId, f: impl FnOnce(&MessageVTable) -> R) -> R {
    MESSAGE_BY_LTID.with(|map| f(map.get(&ltid).expect("invalid LTID")))
}