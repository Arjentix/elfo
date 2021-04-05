use std::{fmt, marker::PhantomData};

use futures_intrusive::sync::GenericManualResetEvent;
use parking_lot::{Mutex, RawMutex};
use slotmap::{new_key_type, Key, SlotMap};
use smallvec::SmallVec;

use crate::{addr::Addr, address_book::AddressBook, envelope::Envelope};

pub(crate) struct RequestTable {
    owner: Addr,
    notifier: GenericManualResetEvent<RawMutex>,
    requests: Mutex<SlotMap<RequestId, RequestInfo>>,
}

assert_impl_all!(RequestTable: Sync);

type Data = SmallVec<[Option<Envelope>; 1]>;

#[derive(Default)]
struct RequestInfo {
    remainder: usize,
    data: Data,
}

new_key_type! {
    pub struct RequestId;
}

impl RequestTable {
    pub(crate) fn new(owner: Addr) -> Self {
        Self {
            owner,
            notifier: GenericManualResetEvent::new(false),
            requests: Mutex::new(SlotMap::default()),
        }
    }

    pub(crate) fn new_request(&self, book: AddressBook) -> ResponseToken<()> {
        let mut requests = self.requests.lock();
        let request_id = requests.insert(RequestInfo {
            remainder: 1,
            data: Data::new(),
        });
        ResponseToken::new(self.owner, request_id, book)
    }

    pub(crate) fn clone_token(&self, token: &ResponseToken<()>) -> Option<ResponseToken<()>> {
        debug_assert_eq!(token.sender, self.owner);
        let mut requests = self.requests.lock();
        requests.get_mut(token.request_id)?.remainder += 1;
        let book = token.book.clone();
        Some(ResponseToken::new(token.sender, token.request_id, book))
    }

    pub(crate) fn respond(&self, mut token: ResponseToken<()>, envelope: Envelope) {
        self.resolve(token.sender, token.request_id, Some(envelope));
        token.forget();
    }

    pub(crate) async fn wait(&self, request_id: RequestId) -> Data {
        loop {
            self.notifier.wait().await;

            {
                let mut requests = self.requests.lock();
                let request = requests.get(request_id).expect("unknown request");

                if request.remainder == 0 {
                    let info = requests.remove(request_id).expect("under lock");

                    // TODO: use another approach.
                    if requests.values().all(|info| info.remainder != 0) {
                        self.notifier.reset();
                    }

                    break info.data;
                }
            }

            tokio::task::yield_now().await;
        }
    }

    fn resolve(&self, sender: Addr, request_id: RequestId, envelope: Option<Envelope>) {
        // TODO: should we have another strategy for panics?
        debug_assert_eq!(sender, self.owner);
        let mut requests = self.requests.lock();
        let request = requests.get_mut(request_id).expect("unknown request");
        request.data.push(envelope);
        request.remainder -= 1;
        if request.remainder == 0 {
            self.notifier.set();
        }
    }
}

#[must_use]
pub struct ResponseToken<T> {
    pub(crate) sender: Addr,
    pub(crate) request_id: RequestId,
    book: AddressBook,
    marker: PhantomData<T>,
}

impl ResponseToken<()> {
    pub(crate) fn new(sender: Addr, request_id: RequestId, book: AddressBook) -> Self {
        Self {
            sender,
            request_id,
            book,
            marker: PhantomData,
        }
    }

    pub(crate) fn into_typed<T>(mut self) -> ResponseToken<T> {
        let token = ResponseToken {
            sender: self.sender,
            request_id: self.request_id,
            book: self.book.clone(),
            marker: PhantomData,
        };
        self.forget();
        token
    }
}

impl<R> ResponseToken<R> {
    pub(crate) fn forgotten(book: AddressBook) -> Self {
        Self {
            sender: Addr::NULL,
            request_id: RequestId::null(),
            book,
            marker: PhantomData,
        }
    }

    pub(crate) fn into_untyped(mut self) -> ResponseToken<()> {
        let token = ResponseToken {
            sender: self.sender,
            request_id: self.request_id,
            book: self.book.clone(),
            marker: PhantomData,
        };
        self.forget();
        token
    }

    pub(crate) fn is_forgotten(&self) -> bool {
        self.request_id == RequestId::null()
    }

    fn forget(&mut self) {
        self.request_id = RequestId::null();
    }
}

impl<T> Drop for ResponseToken<T> {
    fn drop(&mut self) {
        // We use the special value of `RequestId` to reduce memory usage.
        if self.request_id.is_null() {
            return;
        }

        let object = ward!(self.book.get(self.sender));
        let actor = ward!(object.as_actor());
        actor
            .request_table()
            .resolve(self.sender, self.request_id, None);
    }
}

impl<T> fmt::Debug for ResponseToken<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseToken").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use elfo_macros::message;

    use crate::{assert_msg_eq, envelope::MessageKind};

    #[message(elfo = crate)]
    #[derive(PartialEq)]
    struct Num(u32);

    fn envelope(addr: Addr, num: Num) -> Envelope {
        Envelope::new(num, MessageKind::Regular { sender: addr }).upcast()
    }

    #[tokio::test]
    async fn one_request_one_response() {
        let addr = Addr::from_bits(1);
        let table = Arc::new(RequestTable::new(addr));
        let book = AddressBook::new();

        for _ in 0..3 {
            let token = table.new_request(book.clone());
            let request_id = token.request_id;

            let table1 = table.clone();
            tokio::spawn(async move {
                table1.respond(token, envelope(addr, Num(42)));
            });

            let mut data = table.wait(request_id).await;

            assert_eq!(data.len(), 1);
            assert_msg_eq!(data.pop().unwrap().unwrap(), Num(42));
        }
    }

    #[tokio::test]
    async fn one_request_many_response() {
        let addr = Addr::from_bits(1);
        let table = Arc::new(RequestTable::new(addr));
        let token = table.new_request(AddressBook::new());
        let request_id = token.request_id;

        let n = 5;
        for i in 1..n {
            let table1 = table.clone();
            let token = table.clone_token(&token).unwrap();
            tokio::spawn(async move {
                table1.respond(token, envelope(addr, Num(i)));
            });
        }

        table.respond(token, envelope(addr, Num(0)));

        let mut data = table.wait(request_id).await;
        assert_eq!(data.len(), n as usize);

        for (i, envelope) in data.drain(..).enumerate() {
            assert_msg_eq!(envelope.unwrap(), Num(i as u32));
        }
    }

    // TODO: check many requests.
    // TODO: check `Drop`.
}
