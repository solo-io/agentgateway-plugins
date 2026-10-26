// Types for code run by the agentgateway quickjs plugin.
//
// Define `onRequest` and/or `onResponse` at the top level of `config.code`
// (see `OnRequest` and `OnResponse` below). Both may be async. TypeScript
// types are stripped before the code runs, so they're for editors only and
// aren't checked at runtime.

/** A header name and value. Repeated headers appear once per value. */
type HeaderEntry = [name: string, value: string];

/** Operations shared by requests and responses. */
interface Message {
	/** First value of a header, or `undefined`. Names are case-insensitive. */
	header(name: string): string | undefined;
	/** Every header, in order. */
	headers(): HeaderEntry[];
	/** Replaces all values of a header. */
	setHeader(name: string, value: string): void;
	/** Appends a header value. */
	addHeader(name: string, value: string): void;
	removeHeader(name: string): void;
	/**
	 * Reads the whole body as text. Fails if it's larger than `limit` bytes
	 * (default 1 MiB). The body is still forwarded unless you replace it.
	 */
	text(limit?: number): Promise<string>;
	/** Replaces the body. */
	setBody(body: string): void;
	/** Reads a gateway property, e.g. `["source", "address"]`. */
	property(path: string[]): unknown;
	/**
	 * Sets metadata visible to the rest of the gateway through CEL, under the
	 * plugin's configured name (e.g. in access logs).
	 */
	setMetadata(key: string, value: unknown): void;
	/** Evaluates a CEL expression, e.g. `request.headers["user-agent"]`. */
	cel(expression: string): unknown;
}

interface GatewayRequest extends Message {
	readonly method: string | undefined;
	readonly scheme: string | undefined;
	readonly authority: string | undefined;
	/** Path and query, e.g. `/search?q=x`. */
	readonly path: string | undefined;
}

interface GatewayResponse extends Message {
	/** Status code from the upstream. */
	readonly status: number | undefined;
	setStatus(status: number): void;
}

/** Return this to respond directly instead of continuing. */
interface DirectResponse {
	status: number;
	headers?: Record<string, string>;
	body?: string;
}

/** Return this from `onRequest` to pass JSON-serializable state to `onResponse`. */
interface ContinueWithState {
	state: unknown;
}

type RequestResult = void | DirectResponse | ContinueWithState;
type ResponseResult = void | DirectResponse;

/** Signature of `onRequest`, e.g. `const onRequest: OnRequest = async (req) => {}`. */
type OnRequest = (req: GatewayRequest) => RequestResult | Promise<RequestResult>;
/** Signature of `onResponse`. `state` is whatever `onRequest` returned as `{ state }`. */
type OnResponse = (res: GatewayResponse, state: any) => ResponseResult | Promise<ResponseResult>;

/** Waits without blocking the gateway. */
declare function sleep(ms: number): Promise<void>;

interface HttpCallInit {
	/** Defaults to `GET`. */
	method?: string;
	headers?: Record<string, string>;
	body?: string;
	/** Maximum response body size in bytes. Defaults to 1 MiB. */
	bodyLimit?: number;
}

interface HttpCallResponse {
	status: number;
	headers: Record<string, string>;
	body: string;
}

/** Makes an HTTP request through the gateway. `url` must be absolute. */
declare function httpCall(url: string, init?: HttpCallInit): Promise<HttpCallResponse>;

/** Writes to the gateway log. */
declare function log(level: "trace" | "debug" | "info" | "warn" | "error", message: string): void;

/** `console.*` writes to the gateway log at the matching level. */
declare const console: {
	debug(...args: unknown[]): void;
	log(...args: unknown[]): void;
	info(...args: unknown[]): void;
	warn(...args: unknown[]): void;
	error(...args: unknown[]): void;
};
