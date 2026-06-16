# Cashu Pay-to-Extend Storage — Research & Planning

## Problem
BlossomFS needs to pay for blob storage on BUD-07 servers (e.g., blossom.psbt.me charges 1 sat/MB/month). The FUSE filesystem needs a way to make Cashu payments on demand during write operations.

## Architecture Decision: Wally (shared system wallet)

**Decision**: Use Wally (https://github.com/Origami74/wally) as a shared system wallet daemon, NOT embed CDK directly in BlossomFS.

**Rationale**:
- Single Cashu wallet for all services on the machine (BlossomFS, routstr, tollgate.me, etc.)
- BlossomFS focuses on filesystem + Blossom protocol, not wallet management
- Maintainer has confirmed plans to expand Wally for multi-service use
- Avoids key management, mint rotation, token storage complexity in BlossomFS

## BUD-07 Payment Flow

### Live test server: blossom.psbt.me
- **Free tier**: Files under 1 MB, 30-day retention
- **Paid tier**: 1 sat/MB/month (min 2 sats)
- **Payment**: Cashu via NUT-24 (HTTP 402 flow)
- **Test mint**: testnut.cashu.exchange (auto-pays Lightning invoices)
- **Extend**: `PATCH /<sha256>` with Cashu to extend retention

### Flow:
1. FUSE write → BlossomFS calls `PUT /upload` with Nostr auth
2. Server returns `402 Payment Required` with:
   - `X-Cashu: creqA<base64url-cbor>` — payment request `{a: amount, u: "sat", m: [mint_url]}`
   - `X-Price-Sats: 2` — price in sats
   - `X-Expiry-Days: 7` — retention days
3. BlossomFS calls Wally: "pay {amount} sats to {mint_url}"
4. Wally returns `cashuB<base64url>` payment proof token
5. BlossomFS retries `PUT /upload` with `X-Cashu: cashuB...` header
6. Server accepts, returns `201 Created` with blob descriptor

### Quota checking (BUD-06):
- `HEAD /upload` with `X-SHA-256`, `X-Content-Type`, `X-Content-Length`
- Returns 200 OK (free) or 402 (payment required)
- Use for pre-flight check before large uploads

## Wally Integration (pending API research)

Wally research in progress. Key questions:
- HTTP API? Unix socket? CLI?
- How does a client request: "pay N sats to this mint"?
- Does it handle the full NUT-24 flow (decode creqA, mint/swap, produce cashuB)?
- What's the configuration model?

### Integration approach (assumed HTTP API):
```rust
// In BlossomFS upload handler, on 402:
async fn handle_payment_required(
    response: &Response,
    wally_url: &str,
) -> Result<String> {
    let creq = response.headers().get("X-Cashu")?;
    // Call Wally to pay
    let wally_resp = reqwest::Client::new()
        .post(format!("{wally_url}/pay"))
        .json(&serde_json::json!({
            "request": creq,
        }))
        .send().await?;
    let token = wally_resp.json::<WallyResponse>().await?.token;
    Ok(token) // cashuB token
}
```

### CLI flag:
```
--wally-url <URL>     Wally wallet daemon URL (e.g., http://localhost:28332)
--auto-pay-threshold <sats>  Auto-pay below this amount (default: 1000)
```

### Error handling:
- If Wally is unavailable → log error, return EIO to FUSE
- If payment fails (insufficient balance) → log error, return ENOSPC to FUSE
- If amount > auto-pay threshold → log warning (no interactive prompt in FUSE)

## Implementation phases

### Phase 1: Read-only BUD-07 server support (no payment)
- Mount blossom.psbt.me as a Blossom server
- List blobs via BUD-12
- Read/download blobs (always free)
- No payment needed for read operations

### Phase 2: Wally integration for uploads
- Detect 402 response on `PUT /upload`
- Call Wally to make payment
- Retry with payment proof
- Configurable via `--wolly-url`

### Phase 3: Quota management
- Pre-flight check via BUD-06 `HEAD /upload`
- Local quota cache (avoid round-trips)
- `PATCH /<sha256>` to extend retention
- Display quota info in STATUS.txt

## References
- BUD-07 spec: https://github.com/hzrd149/blossom/blob/master/buds/07.md
- NUT-24 spec: https://github.com/cashubtc/nuts/blob/main/24.md
- blossom.psbt.me: https://blossom.psbt.me/llms.txt
- Wally: https://github.com/Origami74/wally
- CDK (alternative): https://github.com/cashubtc/cdk
- testnut mint: https://testnut.cashu.exchange
