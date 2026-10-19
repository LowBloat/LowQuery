import { StreamLanguage, LanguageSupport } from "@codemirror/language";
import { completeFromList, type Completion } from "@codemirror/autocomplete";

/** Commands offered by autocompletion (the ones people type by hand most). */
const COMMANDS = `APPEND AUTH BGSAVE BITCOUNT BLMOVE BLPOP BRPOP BZPOPMAX BZPOPMIN CLIENT CONFIG COPY DBSIZE DECR
DECRBY DEL DISCARD DUMP ECHO EVAL EVALSHA EXEC EXISTS EXPIRE EXPIREAT EXPIRETIME FLUSHALL FLUSHDB
GEOADD GEODIST GEOPOS GEOSEARCH GET GETDEL GETEX GETRANGE HDEL HEXISTS HEXPIRE HGET HGETALL HINCRBY
HINCRBYFLOAT HKEYS HLEN HMGET HMSET HRANDFIELD HSCAN HSET HSETNX HSTRLEN HTTL HVALS INCR INCRBY
INCRBYFLOAT INFO JSON.GET JSON.SET JSON.DEL JSON.TYPE KEYS LASTSAVE LINDEX LINSERT LLEN LMOVE LPOP LPOS
LPUSH LPUSHX LRANGE LREM LSET LTRIM MEMORY MGET MONITOR MOVE MSET MSETNX MULTI OBJECT PERSIST PEXPIRE
PEXPIREAT PFADD PFCOUNT PFMERGE PING PSETEX PTTL PUBLISH PUBSUB RANDOMKEY RENAME RENAMENX RPOP
RPOPLPUSH RPUSH RPUSHX SADD SCAN SCARD SCRIPT SDIFF SDIFFSTORE SELECT SET SETEX SETNX SETRANGE SINTER
SINTERCARD SINTERSTORE SISMEMBER SLOWLOG SMEMBERS SMISMEMBER SMOVE SORT SPOP SRANDMEMBER SREM SSCAN
STRLEN SUNION SUNIONSTORE SWAPDB TIME TOUCH TTL TYPE UNLINK UNWATCH WATCH XACK XADD XAUTOCLAIM XCLAIM
XDEL XGROUP XINFO XLEN XPENDING XRANGE XREAD XREADGROUP XREVRANGE XTRIM ZADD ZCARD ZCOUNT ZDIFF
ZINCRBY ZINTER ZINTERCARD ZLEXCOUNT ZMSCORE ZPOPMAX ZPOPMIN ZRANDMEMBER ZRANGE ZRANGEBYSCORE ZRANK ZREM
ZREMRANGEBYRANK ZREMRANGEBYSCORE ZREVRANGE ZREVRANK ZSCAN ZSCORE ZUNION ZUNIONSTORE`.split(/\s+/);

/** Options that read better highlighted like keywords. */
const OPTIONS = new Set(
	"WITHSCORES COUNT MATCH TYPE EX PX EXAT PXAT NX XX GT LT CH INCR LIMIT BYSCORE BYLEX REV KEEPTTL GET WITHVALUES".split(" "),
);

interface State {
	first: boolean;
}

const redis = StreamLanguage.define<State>({
	name: "redis",
	startState: () => ({ first: true }),
	token(stream, state) {
		if (stream.sol()) state.first = true;
		if (stream.eatSpace()) return null;
		if (stream.match("#") || stream.match("//")) {
			stream.skipToEnd();
			return "comment";
		}
		const ch = stream.peek();
		if (ch === '"' || ch === "'") {
			stream.next();
			let escaped = false;
			while (!stream.eol()) {
				const c = stream.next();
				if (escaped) escaped = false;
				else if (c === "\\") escaped = true;
				else if (c === ch) break;
			}
			state.first = false;
			return "string";
		}
		stream.eatWhile(/[^\s"']/);
		const word = stream.current();
		if (state.first) {
			state.first = false;
			return "keyword";
		}
		if (/^[-+]?(\d+(\.\d+)?|inf)$/i.test(word)) return "number";
		if (OPTIONS.has(word.toUpperCase())) return "keyword";
		return null;
	},
	languageData: { commentTokens: { line: "#" } },
});

/** Redis editor support: highlighting plus command and key completion. */
export function redisSupport(keys: string[]): LanguageSupport {
	const options: Completion[] = [
		...COMMANDS.map((c) => ({ label: c, type: "keyword" })),
		...keys.map((k) => ({ label: k, type: "variable", detail: "key" })),
	];
	return new LanguageSupport(redis, redis.data.of({ autocomplete: completeFromList(options) }));
}
