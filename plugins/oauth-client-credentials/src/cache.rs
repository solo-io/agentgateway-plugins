use std::{
	sync::Mutex,
	task::{Poll, Waker},
	time::{Duration, Instant},
};

/// A cached credential and its absolute expiry and refresh times.
#[derive(Clone)]
pub struct Token {
	pub value: String,
	pub expires: Instant,
	pub refresh_at: Instant,
	pub generation: u64,
}

#[derive(Default)]
struct State {
	token: Option<Token>,
	fetching: bool,
	generation: u64,
	waiters: Vec<Waker>,
}

/// Shared state for one token endpoint and client configuration.
#[derive(Default)]
pub struct Cache {
	state: Mutex<State>,
}

/// The action a request should take after inspecting the cache.
pub enum Lookup {
	Token(Token),
	Fetch,
	Wait,
}

impl Cache {
	/// Return a usable token or elect exactly one caller to fetch it.
	pub fn lookup(&self, now: Instant) -> Lookup {
		let Ok(mut state) = self.state.lock() else {
			return Lookup::Wait;
		};
		let valid = state
			.token
			.as_ref()
			.filter(|token| token.expires > now)
			.cloned();
		if let Some(token) = &valid
			&& (state.fetching || now < token.refresh_at)
		{
			return Lookup::Token(token.clone());
		}
		if state.fetching {
			return Lookup::Wait;
		}
		state.fetching = true;
		Lookup::Fetch
	}

	/// Read a token after a completed flight, without starting another refresh.
	pub fn valid(&self, now: Instant) -> Option<Token> {
		self
			.state
			.lock()
			.ok()?
			.token
			.as_ref()
			.filter(|token| token.expires > now)
			.cloned()
	}

	/// Finish a fetch and wake requests that had no usable token to send.
	pub fn complete(
		&self,
		result: Option<(String, Duration, Duration)>,
		now: Instant,
	) -> Option<Token> {
		let Ok(mut state) = self.state.lock() else {
			return None;
		};
		state.fetching = false;
		let token = result.and_then(|(value, ttl, refresh_before)| {
			let expires = now.checked_add(ttl)?;
			let refresh_at = expires.checked_sub(refresh_before.min(ttl / 2))?;
			state.generation = state.generation.wrapping_add(1);
			Some(Token {
				value,
				expires,
				refresh_at,
				generation: state.generation,
			})
		});
		if token.is_some() {
			state.token = token.clone();
		}
		let waiters = std::mem::take(&mut state.waiters);
		drop(state);
		for waiter in waiters {
			waiter.wake();
		}
		token
	}

	/// Register a waiter while a fetch is in flight.
	pub fn poll_wait(&self, waker: &Waker) -> Poll<()> {
		let Ok(mut state) = self.state.lock() else {
			return Poll::Ready(());
		};
		if !state.fetching {
			return Poll::Ready(());
		}
		// Waiters must be woken when the elected fetcher finishes or is dropped;
		// they cannot safely start another call while that fetch is in flight.
		if state.waiters.len() < 4096
			&& !state
				.waiters
				.iter()
				.any(|existing| existing.will_wake(waker))
		{
			state.waiters.push(waker.clone());
		}
		Poll::Pending
	}

	/// Invalidate only the token that actually received a backend 401.
	pub fn invalidate(&self, generation: u64) {
		if let Ok(mut state) = self.state.lock()
			&& state
				.token
				.as_ref()
				.is_some_and(|token| token.generation == generation)
		{
			state.token = None;
		}
	}
}

/// Clears an abandoned flight if the fetcher's request is cancelled.
pub struct Flight<'a>(pub Option<&'a Cache>);

impl Drop for Flight<'_> {
	fn drop(&mut self) {
		if let Some(cache) = self.0 {
			cache.complete(None, Instant::now());
		}
	}
}
