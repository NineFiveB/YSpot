# Code signing

How YSpot's Windows release artifacts get signed, why they must be, and why none
of this helps you build locally.

Everything here is optional infrastructure: `cargo build`, `tauri build` and CI
all work without an Azure account. When signing is unconfigured the wrapper
script prints `signing skipped (unconfigured)` and exits 0.

---

## 1. Why we sign

### Smart App Control hard-blocks unsigned binaries

Smart App Control (SAC) is a machine-wide Code Integrity policy on Windows 11. It
evaluates **every** PE file as it is loaded and blocks anything that is neither
signed by a publisher it trusts nor vouched for by Microsoft's Intelligent
Security Graph (ISG) reputation service.

A blocked launch does not look like a signing error. It surfaces as:

```
os error 4551
```

(`ERROR_VIRUS_INFECTED`, "Operation did not complete successfully because the
file contains a virus or potentially unwanted software"), and Windows writes an
Event ID **3077** to `Microsoft-Windows-CodeIntegrity/Operational`.

Consequences for what we ship:

- **Every** PE in the shipped set must be signed: the app executable,
  `yspot-indexd.exe`, every bundled DLL, the WebView2 loader, the NSIS
  uninstaller, and the installer/MSI themselves. SAC checks each one at load
  time; one unsigned DLL is enough to break the app on an enforcing machine.
- The signature must be **RSA**. SAC's signature check does not support ECC.
- The signature must be **timestamped** (see §5).

`scripts/sac-status.ps1` reports whether the local machine is enforcing and what
it has blocked recently.

### SmartScreen is a different subsystem, and signing does not instantly satisfy it

Microsoft Defender SmartScreen is *not* SAC. It is a separate reputation service
that gates the "Windows protected your PC" dialog on download-and-run. Signing
helps it, but only indirectly:

- Reputation accrues to the **publisher identity** over time and download volume.
  A brand-new certificate starts with no reputation, so early releases can still
  trip the SmartScreen prompt even though they are correctly signed.
- Extended Validation (EV) certificates **no longer buy an instant SmartScreen
  bypass**; Microsoft removed that behaviour in 2024. Do not buy an EV cert
  expecting to skip the reputation ramp.

So: signing is a hard requirement for SAC, and a slow-acting improvement for
SmartScreen. Budget for the SmartScreen prompt on the first few releases.

---

## 2. Why signing does **not** fix local development

This is the part people get wrong, so it is spelled out.

The binaries SAC blocks during a normal `cargo` build are not artifacts we could
sign even if we wanted to:

- **Build scripts.** Cargo compiles `build.rs` to
  `target\debug\build\<pkg>-<hash>\build-script-build.exe` and executes it
  *within the same cargo invocation*. There is no hook, no intermediate step, and
  no configuration point between "link this executable" and "run this
  executable".
- **Proc-macro DLLs.** `rustc.exe` loads `target\debug\deps\<macro>-<hash>.dll`
  directly. On this repo's machine the observed 3077 blocks include
  `serde_derive-<hash>.dll` with `rustc.exe` as the launching process.
- **Test binaries** under `target\debug\deps\`.

Cargo's `target.<triple>.runner` setting does **not** help: it only wraps
`cargo run`, `cargo test` and `cargo bench`. It never applies to build-script
execution or to proc-macro loading.

Even if a hook existed, signing intermediates would be absurd on cost: the Basic
tier includes 5,000 signatures per month, and a single clean debug build of this
workspace produces hundreds of build-script executables and proc-macro DLLs.

Also worth knowing:

- SAC's policy is authored by Microsoft and **cannot be customised**. There is no
  supported way to add a path-based allow rule for your source tree.
- A Microsoft Defender exclusion does not create SAC trust. They are different
  subsystems (SAC consults ISG/Defender *reputation*, which is not something you
  can grant yourself).

### What to do instead

1. **Turn Smart App Control Off on your dev machine.**
   Settings → Privacy & security → Windows Security → App & browser control →
   Smart App Control settings → Off.

   Historically this was a one-way door: once Off, the only way back On was a
   Windows reinstall. **KB5079391 changed that**; on builds at or after
   **26200.8116** you can switch SAC back On from the same Settings page.
   `scripts/sac-status.ps1` prints whether your build meets that baseline before
   you flip the switch.

2. **Keep verifying that shipped bits would pass**, by running Microsoft's Smart
   App Control audit policy instead of enforcement. Download the policy bundle
   from <https://aka.ms/sacauditpolicies> and deploy
   **`SmartAppControlAuditNoISG.bin`**.

   - The `NoISG` variant deliberately removes the Intelligent Security Graph
     allowance, so trust comes from signatures alone. That is exactly the
     question you want answered: "would this binary pass on an enforcing machine
     purely on its signature?"
   - Audited-but-would-have-been-blocked loads land as Event ID **3076** in
     `Microsoft-Windows-CodeIntegrity/Operational`. `scripts/sac-status.ps1`
     shows 3076 and 3077 side by side.
   - Deployment is the standard WDAC procedure (copy the policy into
     `C:\Windows\System32\CodeIntegrity\CiPolicies\Active\` and reboot, or use
     `CiTool.exe --update-policy <file>` from an elevated prompt). Follow the
     instructions shipped with the download rather than this paragraph if they
     disagree — the exact steps are Microsoft's to define, not ours.

3. If you would rather not disable SAC at all, build in a VM or container where
   it is not enforcing.

---

## 3. What must be provisioned in Azure

The service is **Azure Artifact Signing** (GA name; formerly "Trusted Signing").
The resource provider is `Microsoft.CodeSigning`, and billing meters still read
"Trusted Signing" — that is expected, not a mistake.

Checklist, in order:

- [ ] **A paid Azure subscription.** Free, trial and sponsored subscriptions are
      rejected for Artifact Signing. Pay-as-you-go or an EA/CSP subscription.
- [ ] **Register the resource provider** `Microsoft.CodeSigning` on that
      subscription.
- [ ] **Create an Artifact Signing account.** Tier **Basic**: about
      **$9.99/month**, includes **5,000 signatures**, overage **$0.005** per
      signature. Note its **region** — you will need it forever (see §4).
- [ ] **Complete identity validation** for the account (see §5). Nothing can be
      signed until this succeeds.
- [ ] **Create a certificate profile** of type **Public Trust**.
      - **Not** "Public Trust Test". Test profiles are issued with the lifetime
        signing EKU `1.3.6.1.4.1.311.10.3.13` and chain to a test root; they are
        **not publicly trusted** and will not satisfy SAC on a customer machine.
      - **Not** "Private Trust" — that is for signing inside your own WDAC/CI
        policy, not for public distribution.
      - Create it in the **same region** as the account.
- [ ] **Assign the two RBAC roles that Owner and Contributor do _not_ imply.**
      Being subscription Owner is not enough for either of these:
      - **Artifact Signing Identity Verifier** — required to start/manage
        identity validation. This role can only be assigned **through the Azure
        portal**, and the assignee needs at least **Reader** at *subscription*
        scope for the portal flow to work.
      - **Artifact Signing Certificate Profile Signer** — required by whatever
        identity actually signs (the GitHub OIDC app registration and/or the
        service principal). Scope it to the certificate profile or the account.
- [ ] **Create the app registration / service principal** used by CI, give it the
      Certificate Profile Signer role, and add a **federated credential** for
      GitHub OIDC (see §7).

---

## 4. Region pinning — get this right the first time

The endpoint URI encodes a region, for example
`https://eus.codesigning.azure.net` (East US).

**The endpoint must match the region of _both_ the signing account _and_ the
certificate profile.** A mismatch is the documented cause of `403` responses and
`SignerSign() failed` errors during signing, and the error message does not point
at the region, so it costs hours to diagnose.

Equally important: Artifact Signing resources **cannot be migrated** afterwards —
not to another subscription, not to another tenant, not to another resource
group. If you put the account in the wrong place, the fix is to delete and
recreate it, which means redoing identity validation. Decide the subscription,
tenant, resource group and region before you click Create.

---

## 5. Identity validation and the certificate subject

**The certificate subject is not something we choose.** Azure derives it from the
validated identity attached to the billing account — legal name and address — and
exposes it **read-only** on the certificate profile. You cannot edit the CN, O,
L, S or C fields.

The practical consequence: **to publish under an organization name, the validated
Azure billing identity must be that organization.** Signing up personally and
typing an organization name somewhere does not work; the subject follows the
validated entity, so a personal billing identity produces a personally-named
certificate.

### Publishing as the organization

**Decision: YSpot signs under the organization identity, never a personal one.**
Certificate profiles and identity validation on the signing account point at the
organization; all future YSpot binaries must carry that publisher.

**Live configuration** (verified against Azure on 2026-09-01):

| Setting | Value | Where it lives |
|---|---|---|
| Signing account | `aegiosot` (Basic SKU, `eastus`) | RG `ytile-signing`, subscription `NineFiveB` |
| Endpoint | `https://eus.codesigning.azure.net/` | repo var `YSPOT_SIGN_ENDPOINT` |
| Certificate profile | `release-signing` | repo var `YSPOT_SIGN_PROFILE` |
| Signing identity | user-assigned MI `ytile-release-signer` | repo var `YSPOT_AZURE_CLIENT_ID` |
| Federated credential | `github-yspot-release-org` → `repo:NineFiveB@323532061/YSpot@1350372443:environment:release` (plus a plain-name form, and the pre-transfer `AegiosOT` one retained) | on that MI |
| Environment gate | `environment: release` on the release job | `.github/workflows/release.yml` |

The account name (`aegiosot`) is **not** part of the certificate — it is just an
Azure resource name. Only the validated identity determines CN and O.

> **Precondition before the first release:** the account currently has **no
> certificate profile**. `release-signing` must exist and be bound to the
> organization identity validation, or every signing step fails. Verify with:
>
> ```
> az rest --method get --url "https://management.azure.com/subscriptions/<sub>/resourceGroups/ytile-signing/providers/Microsoft.CodeSigning/codeSigningAccounts/aegiosot/certificateProfiles?api-version=2026-05-15-preview"
> ```
>
> Also confirm the profile type is **Public Trust**, not Public Trust Test.

**Do not use the "Azure for Students" subscription** for signing — sponsored,
free and trial subscriptions are rejected by Artifact Signing. The signing
account lives in the `NineFiveB` subscription and must stay there (resources
cannot be migrated across subscriptions, tenants or resource groups).

**Verification is enforced, not assumed.** `scripts/verify-signature.ps1` runs
after signing in the release workflow and fails the build if a shipped file is
unsigned, has an invalid signature, or has a subject that does not match
`YSPOT_EXPECTED_SIGNER_CN`. Set that variable to the organization's exact CN once
the certificate profile exists — that is what mechanically prevents a personal
identity from shipping again.

The constraints below are verified against the CA/Browser Forum Baseline
Requirements, Microsoft Learn, the `az` CLI reference, and real certificates
inspected on a developer machine. Do **not** treat any of it as changeable by
argument — the naming rules are CA industry requirements, not Microsoft
preferences.

**The certificate subject cannot be chosen.**

- CN (common name) **must** be the subject's legal name — CSBR v3.11 §7.1.4.2.2.a:
  *"This field MUST contain the Subject's legal name."*
- O (organization) is set to the same value. The portal exposes them as a single
  control literally named "Verified CN and O".
- The Artifact Signing FAQ states it directly: *"No, you can't use a custom
  Common Name (CN) or a custom Organization (O)."*

So `NineFiveB` can appear only if it is the registered legal name of a real
entity. **A DBA does not substitute — it appends.** A real observed example:
`CN="Denver Technologies, Inc. dba Warp"`. A fictitious-business-name filing over
a sole proprietorship therefore yields `<owner's legal name> dba NineFiveB`,
which defeats the point.

**What lands in the shipped binary, and what does not:**

| Field | In the binary? | Controllable? |
|---|---|---|
| CN, O | yes | no — the validated legal name |
| L (city), S (state), C (country) | **yes, always** | **no** — `--include-city/-state/-country` are documented as *private trust only* |
| STREET (street address) | no, by default | yes — `--include-street-address` defaults to `False`; leave it off |
| PC (postal code) | no, by default | yes — `--include-postal-code` defaults to `False`; leave it off |
| email | never | n/a |

A home **street** address therefore stays out of the certificate automatically,
but city and state cannot be suppressed on Public Trust. Expect
`CN=<entity>, O=<entity>, L=<city>, S=<state>, C=<country>` in every signed file.
(A traditional CA can do better on this one axis: CSBR §7.1.4.2.3.c makes
localityName optional when stateOrProvinceName is present, and Sectigo-issued
certs in the wild do omit the city.)

**A durable per-identity EKU** is embedded in every certificate and is stable
across the daily rotation, permanently linking all binaries signed under one
identity validation. Starting a new identity validation issues a new EKU and
**resets accumulated publisher reputation** — so switching identities is not free.

**Organization validation requires all of:**

- A genuinely registered legal entity (articles of incorporation / business
  registration / charter).
- A website on a domain the entity owns.
- **Two** contact mailboxes on that same owned domain — a `gmail.com` address
  will not pass.
- A business identifier (state entity number and/or EIN).
- The entity's business address, which CSBR §3.2.2.1.1 requires to be its
  *"address of existence or operation"* — note that CAs publish policies
  rejecting virtual offices, PO boxes, care-of and mail-forwarding addresses.
- A named human still completes a government-photo-ID check (AU10TIX / Entra
  Verified ID). That person's name is seen by Microsoft but does **not** reach
  the certificate subject.

**Two risks worth knowing before starting:**

1. **Three documentation attempts, then onboarding closes permanently.** This is
   not theoretical — properly registered companies have been rejected three times
   and permanently closed out, with no reason given by the portal.
2. **Microsoft's docs contradict each other on organization age.** The MSIX
   package-signing overview states organizations must have *"a verifiable tax
   history of three or more years"*; the Artifact Signing quickstart states no
   such rule and a Microsoft employee has said there is no minimum. Treat a
   brand-new entity as elevated risk, not a clear pass.

**Sequencing that avoids a self-inflicted outage:** identity validations are
subscription-scoped and can coexist. Keep any existing working validation alive
and keep shipping under it until the organization validation reaches *Completed*.
Deleting the working one first leaves no way to sign anything.

**Fallback if Microsoft declines:** an OV certificate from Sectigo/DigiCert issued
to the entity. Functionally equivalent for Smart App Control and SmartScreen (EV
lost its SmartScreen bypass in 2024), and it can omit the city. The cost is an
HSM/USB token requirement since 2023, which is materially worse for automated CI.

Checklist to complete when the entity exists:

- [ ] Legal entity name exactly as it will appear in the certificate subject
- [ ] Which Azure billing account / subscription that entity owns
- [ ] Who holds the **Artifact Signing Identity Verifier** role
- [ ] Certificate Subject Preview confirmed before submitting
- [ ] Date validation was submitted / approved, and the resulting expiry date
      (put the 60-day renewal window in a calendar — see §6)

Until that is done, the workflow's signing steps stay disabled and releases go
out unsigned (and will be blocked by SAC on enforcing machines).

### Timestamping is mandatory

Artifact Signing issues **short-lived leaf certificates: renewed daily, valid
about 72 hours.** An un-timestamped signature therefore stops validating roughly
three days after it is produced.

Every signing path in this repo passes the RFC3161 timestamp URL:

```
http://timestamp.acs.microsoft.com
```

Do not remove it, and do not "optimise" it away when a build is slow.

---

## 6. Renewal operations

- Identity validation **expires**. When it lapses, **all signing stops** — not
  just new validations, everything.
- **Renewal can only be started within 60 days of expiry.** Earlier is not
  allowed, so this cannot be done "well in advance"; it has to be scheduled.
- Renewal requires **deleting and recreating the certificate profile with the
  same name**. Plan for the profile to be briefly absent, and do not run a
  release during that window.
- Certificates already issued and **timestamped** keep validating after renewal.
  This is another reason timestamping is not optional.

Put the renewal window in a shared calendar the moment validation is approved.

---

## 7. How this repo is wired

### Files

| Path | Role |
| --- | --- |
| `scripts/sac-status.ps1` | Diagnostic: SAC state, active CI policies, recent 3076/3077 events. |
| `scripts/sign-windows.ps1` | Signing wrapper. No-op when unconfigured, loud failure when configured and broken. |
| `apps/shell/src-tauri/tauri.conf.json` | `bundle.active: true`, targets `nsis` + `msi`, and `bundle.windows.signCommand` pointing at the wrapper. |
| `.github/workflows/release.yml` | Tag-triggered release build + signing. |

### `signCommand` wiring

```json
"windows": {
  "signCommand": {
    "cmd": "powershell",
    "args": [
      "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
      "-File", "../../../scripts/sign-windows.ps1",
      "-Path", "%1"
    ]
  }
}
```

Details that matter if you edit this:

- The object form (`cmd` + `args`) is used deliberately. The string form splits
  on spaces, which breaks on any path containing a space.
- `%1` must be **its own array element**. Tauri substitutes an argument only when
  it is exactly `%1`.
- The relative script path is resolved against the **`src-tauri` directory**,
  because the Tauri CLI `set_current_dir`s there before bundling. Tauri then
  rewrites existing relative argument paths to absolute ones, which is what makes
  the NSIS `!uninstfinalize` uninstaller-signing hook work from its own
  directory.
- Setting `signCommand` at all is what makes the bundler decide it "can sign";
  no certificate thumbprint is needed.

### Local signing

```powershell
$env:YSPOT_SIGNING_ENABLED = '1'
$env:YSPOT_SIGN_ENDPOINT   = 'https://<region>.codesigning.azure.net'
$env:YSPOT_SIGN_ACCOUNT    = '<artifact signing account name>'
$env:YSPOT_SIGN_PROFILE    = '<certificate profile name>'
$env:AZURE_TENANT_ID       = '<tenant id>'
$env:AZURE_CLIENT_ID       = '<app registration id>'
$env:AZURE_CLIENT_SECRET   = '<client secret>'

cargo install artifact-signing-cli      # NOT trusted-signing-cli (deprecated)
cd apps/shell && npm run tauri -- build
```

Or sign an existing tree directly:

```powershell
./scripts/sign-windows.ps1 -Path target/release -Recurse
```

Optional overrides: `YSPOT_SIGN_DESCRIPTION`, `YSPOT_SIGN_TIMESTAMP_URL`,
`YSPOT_SIGN_CLI`, `SIGNTOOL_PATH`, `AZURE_CLI_PATH`. The wrapper auto-discovers
`signtool.exe` from the installed Windows Kits rather than trusting
`artifact-signing-cli`'s hard-coded default kit version.

### GitHub configuration

Non-secret configuration lives in **repository variables**
(Settings → Secrets and variables → Actions → Variables):

| Variable | Example |
| --- | --- |
| `YSPOT_SIGN_ENDPOINT` | `https://eus.codesigning.azure.net` |
| `YSPOT_SIGN_ACCOUNT` | the Artifact Signing account name |
| `YSPOT_SIGN_PROFILE` | the certificate profile name |
| `YSPOT_AZURE_TENANT_ID` | tenant (directory) id |
| `YSPOT_AZURE_CLIENT_ID` | app registration (client) id |
| `YSPOT_AZURE_SUBSCRIPTION_ID` | subscription id |

Only one **secret** is used, and it is optional:

| Secret | Why |
| --- | --- |
| `YSPOT_AZURE_CLIENT_SECRET` | Enables the in-bundler signing path (§8). Not needed for the OIDC action steps. |

If any of the variables are unset, the workflow logs a warning and produces
**unsigned** artifacts instead of failing. That keeps forks and pre-Azure
releases buildable.

#### OIDC federated credential

`azure/login@v3` runs with `permissions: id-token: write` and exchanges a GitHub
OIDC token, so no long-lived credential is needed for the action-based steps. The
app registration needs a **federated credential** whose subject matches this
workflow. Note that ordinary federated credentials do **not** support wildcards,
and this workflow triggers on `v*` tags, so a subject of
`repo:<owner>/<repo>:ref:refs/tags/v1.2.3` only matches one exact tag.

Pick one:

- Add `environment: release` to the `release` job and register the subject
  `repo:<owner>/<repo>:environment:release` — stable across all tags. This is a
  one-line change to the workflow and is the recommended option.
- Or use the wildcard-capable ("flexible") federated identity credential feature
  if it is available on your tenant.

---

## 8. The binary-patching caveat (read before trusting an installer)

The Tauri bundler patches the main executable with bundle-type information
(`__TAURI_BUNDLE_TYPE_VAR_*`) **once per package type**, immediately before
building that package. Rewriting bytes in a signed PE invalidates its Authenticode
signature.

The bundler handles this correctly *if* a `signCommand` is configured: it
re-signs the executable after every patch. If no `signCommand` is configured, the
copy of the app executable that ends up **inside** the NSIS installer and the MSI
is unsigned — even if you signed `target\release\*.exe` beforehand. (The bundler
restores the original pre-patch bytes afterwards, so the *standalone* executable
on disk still looks correctly signed. This is exactly the trap.)

The same applies to the **NSIS uninstaller**, which is produced during bundling
and can only be signed through the `!uninstfinalize` hook — i.e. through
`signCommand`.

That is why `release.yml` has two signing mechanisms:

| Path | Auth | Covers |
| --- | --- | --- |
| `azure/artifact-signing-action@v2` after `--no-bundle` build | OIDC (`azure/login@v3`) | standalone `yspot-indexd.exe`, the shell exe, loose DLLs |
| `scripts/sign-windows.ps1` via `signCommand` during `tauri bundle` | service principal secret | the app exe **inside** the installer, NSIS plugins, WebView2 loader, resources, the **uninstaller** |
| `azure/artifact-signing-action@v2` after bundling | OIDC | the NSIS setup `.exe` and the `.msi` as files |

Without `YSPOT_AZURE_CLIENT_SECRET`, the middle row does not run and the workflow
emits a warning saying so. The artifacts still install, but the installed
application executable will be blocked by SAC on an enforcing machine.

A future Tauri CLI adds `--no-binary-patching`, which would let the OIDC-only path
cover everything (sign first, then bundle without rewriting). It is **not** in
CLI `2.11.4`, the version pinned in `apps/shell/package-lock.json`. When it ships,
this whole section can be simplified and the client secret dropped.

---

## 9. Troubleshooting

### `az login` and the Windows broker (community leads, not documented procedure)

`artifact-signing-cli` authenticates through the Azure CLI. On Windows, the Web
Account Manager (WAM) broker has been reported to break token acquisition for the
code-signing scope. Two workarounds circulate in issue threads:

```powershell
az config set core.enable_broker_on_windows=false
```

```powershell
az login --use-device-code --scope "https://codesigning.azure.net/.default"
```

Treat both as **community troubleshooting leads**, not as a supported procedure.
Neither is part of Microsoft's documented flow, and either may stop being
necessary.

### `403` / `SignerSign() failed`

Region mismatch between the endpoint and the account/profile is the documented
first suspect. See §4.

### Signing succeeds but the file is still blocked

- Check the algorithm is RSA, not ECC.
- Check the profile is **Public Trust**, not Public Trust Test. A test-profile
  signature carries EKU `1.3.6.1.4.1.311.10.3.13` and does not chain to a public
  root.
- Check the signature is timestamped: `Get-AuthenticodeSignature <file>` and look
  for a non-null `TimeStamperCertificate`. Untimestamped signatures die after
  ~72 hours.

### Something on my machine got blocked and I want to know what

```powershell
pwsh -File scripts/sac-status.ps1 -Count 50
```

Read the **File** column for what was blocked and the **Process** column for what
launched it. The rendered event *message* shows the parent process in
parentheses, which is why these logs are so often misread as "cargo.exe was
blocked" when cargo merely launched the blocked binary. The script parses the
structured `File Name` / `Process Name` event fields instead.

---

## 10. Open maintainer decisions

1. **Ship the client secret, or wait for `--no-binary-patching`?** (§8) Without
   it, the app exe inside the installer is unsigned.
2. **How does `yspot-indexd.exe` get installed?** It is built and signed by the
   release workflow, but it is *not* currently part of the Tauri bundle — there
   is no `bundle.externalBin` or `bundle.resources` entry for it, so the
   installer does not place it on disk.
3. **Executable name.** `mainBinaryName` is unset, so the installed binary is
   `yspot-shell.exe` while the product is presented as "YSpot". Set
   `mainBinaryName` if that should match.
4. **`bundle.publisher`.** Unset, so it defaults to the second segment of the
   bundle identifier (`yspot`). Set it once the publishing entity is settled —
   note this is only installer metadata; the certificate subject is still
   whatever identity validation produced (§5).
5. **Region.** Not chosen yet, and not changeable later (§4).
6. **Federated credential style** — environment-scoped vs flexible/wildcard (§7).
7. **Should the release workflow publish a GitHub Release?** Today it only
   uploads build artifacts; publishing would need `permissions: contents: write`.

---

## Appendix — provenance

Load-bearing names in this setup were checked against upstream sources rather
than recalled, because a wrong key silently does nothing:

- `bundle.windows.signCommand`, its `{cmd, args}` object form, and the `%1`
  substitution rule: `tauri-utils` **2.9.3** `src/config.rs`
  (`CustomSignCommandConfig`, `WindowsConfig`) and `tauri-bundler`
  `windows/sign.rs` — the exact versions in `Cargo.lock`.
- Bundler behaviour (`can_sign()`, patch → sign → restore ordering, uninstaller
  and WebView2-loader signing): `tauri-bundler` **2.9.4** as used by
  `@tauri-apps/cli` **2.11.4**.
- `azure/artifact-signing-action@v2` input names: the `action.yml` at the `v2`
  tag of `Azure/artifact-signing-action`.
- `artifact-signing-cli` flags (`--endpoint/-e`, `--account/-a`,
  `--certificate/-c`, `--fd`, `--tr`, `--td`, `--description/-d`, and the
  required `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` / `AZURE_TENANT_ID`):
  `src/main.rs` of `levminer/trusted-signing-cli`, the repository behind the
  `artifact-signing-cli` crate.
- CodeIntegrity 3077 `EventData` field names (`File Name`, `Process Name`,
  `Status`): read off real events on the development machine.
