# Soulseek Authentication Investigation

## Current Status

✅ **Protocol Version Fixed**: Version 183 accepted (no more "client too old" error)
❌ **Authentication Failing**: Getting "INVALIDPASS" error

## What We've Tried

### Attempt 1: Empty MD5 Hash
```rust
let md5_hash = String::new();
```
**Result**: INVALIDPASS

### Attempt 2: MD5(password)
```rust
let md5_hash = format!("{:x}", md5::compute(password.as_bytes()));
```
**Result**: INVALIDPASS

### Attempt 3: MD5(username + password) - Original
```rust
let combined = format!("{}{}", username, password);
let md5_hash = format!("{:x}", md5::compute(combined.as_bytes()));
```
**Result**: INVALIDPASS (with version 183)

## Possible Causes

### 1. Credential Issues
- **Password incorrect** in `.env` file
- **Username incorrect** or account doesn't exist
- **Account locked** or restricted
- **Special characters** in password not properly handled

### 2. Protocol Implementation Issues
- MD5 hash calculation incorrect for version 183
- Password encoding issue (UTF-8 vs ASCII)
- Message format incorrect
- Additional authentication steps required

### 3. Account Type Issues
- Account may require email verification
- Account may be suspended
- New accounts may have restrictions
- Account may require different authentication method

## Next Steps to Investigate

### Step 1: Verify Credentials
Please verify your Soulseek credentials:

1. **Check `.env` file**:
   ```bash
   cat backend/.env | grep SLSK
   ```

2. **Test credentials manually**:
   - Try logging into Soulseek with official client
   - Try logging into https://www.slsknet.org with web interface
   - Verify username and password work

3. **Check for special characters**:
   - Password with quotes? Escape them properly
   - Password with spaces? Make sure they're included
   - Password with symbols? Verify they're correct

### Step 2: Try Official Client
1. Download Nicotine+ or SoulseekQt
2. Try logging in with the same credentials from `.env`
3. Confirm login works with official client

### Step 3: Check Account Status
- Is the account newly created?
- Has it ever successfully connected before?
- Are there any email verification requirements?
- Check if account shows as online on Soulseek network

### Step 4: Protocol Research
Research needed on:
- Exact MD5 hash format for version 183
- Whether SHA256 or other hash is needed
- Whether additional auth steps are required
- Check Nicotine+ source code for version 183 auth

## Reference: Protocol Versions & Auth

### Version 160 (Old Nicotine+)
- MD5 hash: `md5(username + password)`
- Password: Plain text
- Works: ❌ (rejected as "too old")

### Version 183 (SoulseekQt)
- MD5 hash: ??? (unknown)
- Password: Plain text
- Works: ⚠️ (accepted protocol, auth failing)

## Questions for User

1. **Can you confirm your Soulseek credentials work?**
   - Have you successfully logged into Soulseek recently?
   - Using official client (Nicotine+, SoulseekQt)?

2. **Are the credentials in your `.env` file correct?**
   - No typos in username?
   - No typos in password?
   - Special characters properly formatted?

3. **Account details**:
   - When was the account created?
   - Has it been used recently?
   - Are there any account restrictions?

## Code Location

**File**: `backend/src/protocol/connection.rs:56-83`

Current authentication code:
```rust
pub async fn login(&self, username: &str, password: &str) -> Result<()> {
    let md5_hash = format!("{:x}", md5::compute(password.as_bytes()));

    let login_msg = ServerMessage::Login {
        username: username.to_string(),
        password: password.to_string(),
        version: 183,
        md5_hash,
    };

    self.send(login_msg).await?;

    match self.receive().await? {
        Some(ServerMessage::LoginResponse { success, reason, .. }) => {
            if success {
                tracing::info!("Successfully logged in as {}", username);
                Ok(())
            } else {
                bail!("Login failed: {}", reason);
            }
        }
        Some(other) => bail!("Unexpected response to login: {:?}", other),
        None => bail!("Connection closed during login"),
    }
}
```

## Recommended Actions

1. **Verify credentials work** with official Soulseek client
2. **Check `.env` file** for typos or formatting issues
3. **Try creating a new test account** to rule out account-specific issues
4. **Research Nicotine+ code** for version 183 authentication
5. **Enable debug logging** to see exact bytes being sent

## Debug Logging

To see what's being sent, we can add logging:

```rust
tracing::debug!("Login attempt - username: {}, version: {}, md5_hash: {}",
                username, 183, md5_hash);
```

This will show us the exact values being sent to the server.

## Resources

- Nicotine+ Source: https://github.com/Nicotine-Plus/nicotine-plus
- Soulseek Protocol Docs: (various reverse-engineered docs available)
- SoulseekQt Protocol Reference: (check SoulseekQt client implementation)

## Summary

We've successfully fixed the protocol version issue (183 works), but authentication is failing. This suggests either:

1. **Credentials are incorrect** (most likely)
2. **MD5 hash calculation is wrong** for version 183
3. **Additional auth steps** are required that we're not implementing

The fastest way forward is to **verify your credentials work** with an official Soulseek client.
