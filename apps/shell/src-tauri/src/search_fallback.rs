//! Windows Search passthrough (SPEC.md §3.1, §9.5).
//!
//! The shell's own file-search fallback, for the two cases the custom index
//! cannot serve: a scope the service answers with `107 SCOPE_UNSUPPORTED`
//! (non-NTFS, removable, network), and portable mode, where there is no
//! service at all. §9.5 is explicit that these are not two implementations —
//! this module is the one code path.
//!
//! It lives in the **unelevated shell** on purpose (§3.1): Windows Search
//! resolves drive letters per logon session, authenticates to SMB as the
//! calling user, and does its own per-caller security trimming, none of
//! which work from a `LocalSystem` Session-0 process. Keeping the OLE DB and
//! COM stack out of the elevated service also shrinks its attack surface
//! (§8.1).
//!
//! The route is the documented one: `ISearchManager` → the `SystemIndex`
//! catalog → `ISearchQueryHelper`, which turns a user query into SQL and
//! hands over the connection string for the `Search.CollatorDSO` OLE DB
//! provider; the SQL is then run through OLE DB and the rowset read back.
//!
//! §11 Risk 6 asks that this be isolated behind a trait, because Windows
//! Search is the part of the design most likely to be deprecated under us.
//! [`FileSearch`] is that seam.

use windows::core::{Interface, GUID, PCWSTR, PWSTR};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Search::{
    CSearchManager, IAccessor, ICommandText, IDBCreateCommand, IDBCreateSession, IDBInitialize,
    IDataInitialize, IRowset, ISearchManager, DBACCESSOR_ROWDATA, DBBINDING,
    DBMEMOWNER_CLIENTOWNED, DBPARAMIO_NOTPARAM, DBPART_LENGTH, DBPART_STATUS, DBPART_VALUE,
    DBSTATUS_S_OK, DBTYPE_WSTR, HACCESSOR, MSDAINITIALIZE,
};

use crate::com::Apartment;

/// `DBGUID_DEFAULT` — the dialect for `ICommandText::SetCommandText`. Not
/// exposed by the crate, and a frozen ABI value.
const DBGUID_DEFAULT: GUID = GUID::from_u128(0xc8b521fb_5cf3_11ce_ade5_00aa0044773d);

/// The columns the launcher's row needs, and nothing more: every extra one
/// is another binding to get right and more the indexer has to materialize.
const SELECT_COLUMNS: &str = "System.ItemPathDisplay, System.ItemNameDisplay";
/// Per-column text buffer, in UTF-16 units. A Windows path is capped well
/// below this; anything longer is truncated rather than dropped.
const TEXT_UNITS: usize = 1024;

/// One file the fallback found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    pub path: String,
    pub name: String,
}

#[derive(Debug)]
pub enum SearchError {
    /// The `WSearch` service is not running, so there is nothing to ask.
    /// §3.1: this must surface as "not searchable", never as an empty result.
    Unavailable(String),
    /// It was asked and it failed.
    Failed(String),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::Unavailable(m) => write!(f, "Windows Search is unavailable: {m}"),
            SearchError::Failed(m) => write!(f, "Windows Search query failed: {m}"),
        }
    }
}

/// The seam §11 Risk 6 asks for: the fallback's shape, independent of
/// Windows Search being the thing behind it.
pub trait FileSearch: Send + Sync {
    fn search(&self, query: &str, max: usize) -> Result<Vec<FileHit>, SearchError>;
}

/// Windows Search over the `SystemIndex` catalog.
#[derive(Default)]
pub struct WindowsSearch;

impl FileSearch for WindowsSearch {
    fn search(&self, query: &str, max: usize) -> Result<Vec<FileHit>, SearchError> {
        let query = query.trim();
        if query.is_empty() || max == 0 {
            return Ok(Vec::new());
        }
        // Shell APIs want an apartment, and this runs on a Tauri pool thread.
        let _sta = Apartment::sta();
        let (sql, connection) = build_query(query, max)?;
        run_query(&sql, &connection, max)
    }
}

/// Turn a user query into the SQL Windows Search wants, plus the connection
/// string for the provider that runs it.
fn build_query(query: &str, max: usize) -> Result<(String, String), SearchError> {
    // SAFETY: documented CLSID/interface pairs; every string returned is
    // CoTaskMem-allocated and freed exactly once below.
    unsafe {
        let manager: ISearchManager =
            CoCreateInstance(&CSearchManager, None, CLSCTX_ALL).map_err(|e| {
                // A missing or stopped WSearch is the documented failure
                // here, and §3.1 wants it distinguished from a query error.
                SearchError::Unavailable(format!("search manager unavailable ({e})"))
            })?;
        let catalog = manager
            .GetCatalog(windows::core::w!("SystemIndex"))
            .map_err(|e| SearchError::Unavailable(format!("SystemIndex catalog ({e})")))?;
        let helper = catalog
            .GetQueryHelper()
            .map_err(|e| SearchError::Failed(format!("query helper ({e})")))?;
        helper
            .SetQuerySelectColumns(&windows::core::HSTRING::from(SELECT_COLUMNS))
            .map_err(|e| SearchError::Failed(format!("select columns ({e})")))?;
        helper
            .SetQueryMaxResults(max as i32)
            .map_err(|e| SearchError::Failed(format!("max results ({e})")))?;
        let sql = helper
            .GenerateSQLFromUserQuery(&windows::core::HSTRING::from(query))
            .map_err(|e| SearchError::Failed(format!("generate sql ({e})")))?;
        let connection = helper
            .ConnectionString()
            .map_err(|e| SearchError::Failed(format!("connection string ({e})")))?;
        let (sql, connection) = (take_cotaskmem(sql), take_cotaskmem(connection));
        // The generated SQL embeds the user's raw query verbatim, so it is
        // not logged. The SELECT list is fixed by the caller above, which
        // leaves the length as the only part that says anything — enough to
        // tell "the helper produced nothing" from "the provider refused it",
        // which is what this line was for.
        log::debug!("windows search sql: {} chars", sql.len());
        Ok((sql, connection))
    }
}

/// Decode and free a COM-allocated string.
///
/// # Safety
/// `p` must be a NUL-terminated `CoTaskMemAlloc`'d string, unused afterwards.
unsafe fn take_cotaskmem(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: NUL-terminated per the contract.
    let s = unsafe { p.to_string() }.unwrap_or_default();
    // SAFETY: allocated by the API with CoTaskMemAlloc; freed once.
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// One row's worth of bound columns.
///
/// `repr(C)` because OLE DB writes into this by byte offset: the bindings
/// below name `obStatus`, `obLength` and `obValue` as offsets into exactly
/// this layout, so the field order and padding are part of the contract.
#[repr(C)]
#[derive(Clone, Copy)]
struct BoundText {
    status: u32,
    _pad: u32,
    length: usize,
    value: [u16; TEXT_UNITS],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BoundRow {
    columns: [BoundText; 2],
}

impl BoundRow {
    fn zeroed() -> BoundRow {
        // SAFETY: every field is a plain integer or an array of them, for
        // which all-zero is a valid value.
        unsafe { std::mem::zeroed() }
    }

    /// The text of column `i`, or `None` when the provider reported no value.
    fn text(&self, i: usize) -> Option<String> {
        let col = &self.columns[i];
        if col.status != DBSTATUS_S_OK.0 as u32 {
            return None;
        }
        // `length` is bytes, and the value is UTF-16.
        let units = (col.length / 2).min(TEXT_UNITS);
        Some(String::from_utf16_lossy(&col.value[..units]))
    }
}

fn binding(ordinal: usize, column: usize) -> DBBINDING {
    let base = column * std::mem::size_of::<BoundText>();
    DBBINDING {
        iOrdinal: ordinal,
        obStatus: base + std::mem::offset_of!(BoundText, status),
        obLength: base + std::mem::offset_of!(BoundText, length),
        obValue: base + std::mem::offset_of!(BoundText, value),
        pTypeInfo: std::mem::ManuallyDrop::new(None),
        pObject: std::ptr::null_mut(),
        pBindExt: std::ptr::null_mut(),
        dwPart: DBPART_VALUE.0 as u32 | DBPART_LENGTH.0 as u32 | DBPART_STATUS.0 as u32,
        dwMemOwner: DBMEMOWNER_CLIENTOWNED.0 as u32,
        eParamIO: DBPARAMIO_NOTPARAM.0 as u32,
        cbMaxLen: TEXT_UNITS * 2,
        dwFlags: 0,
        wType: DBTYPE_WSTR.0 as u16,
        bPrecision: 0,
        bScale: 0,
    }
}

/// Run the SQL through the `Search.CollatorDSO` OLE DB provider and read the
/// rowset back.
fn run_query(sql: &str, connection: &str, max: usize) -> Result<Vec<FileHit>, SearchError> {
    // SAFETY: a straight-line OLE DB session — every interface is released
    // by its RAII wrapper, the accessor is released before the rowset goes,
    // and each batch's row handles are released before the next.
    unsafe {
        let initializer: IDataInitialize =
            CoCreateInstance(&MSDAINITIALIZE, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| SearchError::Unavailable(format!("OLE DB initializer ({e})")))?;
        let conn_w: Vec<u16> = connection
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut source: Option<windows::core::IUnknown> = None;
        initializer
            .GetDataSource(
                None,
                CLSCTX_ALL.0,
                PCWSTR(conn_w.as_ptr()),
                &IDBInitialize::IID,
                &mut source,
            )
            .map_err(|e| SearchError::Unavailable(format!("data source ({e})")))?;
        let source = source.ok_or_else(|| SearchError::Unavailable("no data source".into()))?;
        let init: IDBInitialize = source
            .cast()
            .map_err(|e| SearchError::Failed(format!("IDBInitialize ({e})")))?;
        init.Initialize()
            .map_err(|e| SearchError::Unavailable(format!("initialize ({e})")))?;

        let session_factory: IDBCreateSession = init
            .cast()
            .map_err(|e| SearchError::Failed(format!("IDBCreateSession ({e})")))?;
        let session = session_factory
            .CreateSession(None, &IDBCreateCommand::IID)
            .map_err(|e| SearchError::Failed(format!("create session ({e})")))?;
        let command_factory: IDBCreateCommand = session
            .cast()
            .map_err(|e| SearchError::Failed(format!("IDBCreateCommand ({e})")))?;
        let command = command_factory
            .CreateCommand(None, &ICommandText::IID)
            .map_err(|e| SearchError::Failed(format!("create command ({e})")))?;
        let text: ICommandText = command
            .cast()
            .map_err(|e| SearchError::Failed(format!("ICommandText ({e})")))?;

        let sql_w: Vec<u16> = sql.encode_utf16().chain(std::iter::once(0)).collect();
        text.SetCommandText(&DBGUID_DEFAULT, PCWSTR(sql_w.as_ptr()))
            .map_err(|e| SearchError::Failed(format!("set command text ({e})")))?;
        let mut rows_affected = 0isize;
        let mut rowset: Option<windows::core::IUnknown> = None;
        text.Execute(
            None,
            &IRowset::IID,
            None,
            Some(&mut rows_affected),
            Some(&mut rowset),
        )
        .map_err(|e| SearchError::Failed(format!("execute ({e})")))?;
        let Some(rowset) = rowset else {
            return Ok(Vec::new());
        };
        let rowset: IRowset = rowset
            .cast()
            .map_err(|e| SearchError::Failed(format!("IRowset ({e})")))?;
        read_rows(&rowset, max)
    }
}

/// # Safety
/// `rowset` must be a live OLE DB rowset over [`SELECT_COLUMNS`].
unsafe fn read_rows(rowset: &IRowset, max: usize) -> Result<Vec<FileHit>, SearchError> {
    // SAFETY: the accessor describes exactly `BoundRow`, and is released
    // before this returns; row handles are released per batch.
    unsafe {
        let accessor: IAccessor = rowset
            .cast()
            .map_err(|e| SearchError::Failed(format!("IAccessor ({e})")))?;
        // Ordinals are 1-based and follow the SELECT order.
        let bindings = [binding(1, 0), binding(2, 1)];
        let mut haccessor = HACCESSOR(0);
        accessor
            .CreateAccessor(
                DBACCESSOR_ROWDATA.0 as u32,
                bindings.len(),
                bindings.as_ptr(),
                std::mem::size_of::<BoundRow>(),
                &mut haccessor,
                None,
            )
            .map_err(|e| SearchError::Failed(format!("create accessor ({e})")))?;

        let mut out = Vec::with_capacity(max.min(64));
        const BATCH: usize = 32;
        let mut fetch_error: Option<String> = None;
        while out.len() < max {
            // `GetNextRows` takes `HROW**`: the slice's LENGTH is the row
            // count being asked for, and the provider writes a pointer to
            // its own array of row handles into the slice's FIRST element.
            // Reading the slice itself as handles is the mistake this shape
            // invites — the provider then reports "row handle is invalid"
            // for every row it just returned.
            let want = BATCH.min(max - out.len());
            let mut rows: [*mut usize; 1] = [std::ptr::null_mut()];
            let mut obtained = 0usize;
            // A slice of `want` nulls: length carries the count, and the
            // first element receives the array pointer.
            let mut request = vec![std::ptr::null_mut::<usize>(); want];
            // DB_S_ENDOFROWSET is a SUCCESS code with zero rows, so the loop
            // ends on the count; a real failure is reported rather than
            // looking like "no matches", which §3.1 is explicit about.
            if let Err(e) = rowset.GetNextRows(0, 0, &mut obtained, &mut request) {
                fetch_error.get_or_insert(format!("fetch rows ({e})"));
                break;
            }
            rows[0] = request[0];
            if obtained == 0 || rows[0].is_null() {
                break;
            }
            let handles = std::slice::from_raw_parts(rows[0], obtained);
            for &handle in handles {
                let mut row = BoundRow::zeroed();
                match rowset.GetData(handle, haccessor, &mut row as *mut BoundRow as *mut _) {
                    Ok(()) => {
                        if let Some(path) = row.text(0) {
                            let name = row.text(1).unwrap_or_else(|| {
                                path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string()
                            });
                            out.push(FileHit { path, name });
                        }
                    }
                    Err(e) => {
                        fetch_error.get_or_insert(format!("read row ({e})"));
                    }
                }
            }
            // Row handles are reference counted: every batch is released
            // before the next is fetched, or the next fetch fails outright
            // with DB_E_ROWSNOTRELEASED.
            if let Err(e) = rowset.ReleaseRows(
                obtained,
                rows[0],
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) {
                fetch_error.get_or_insert(format!("release rows ({e})"));
            }
            // The handle array itself was allocated by the provider.
            windows::Win32::System::Com::CoTaskMemFree(Some(rows[0] as *const _));
        }

        let _ = accessor.ReleaseAccessor(haccessor, None);
        // A read that produced nothing AND hit an error is a failure, not an
        // empty result set.
        match fetch_error {
            Some(e) if out.is_empty() => Err(SearchError::Failed(e)),
            Some(e) => {
                log::warn!("windows search: partial results ({e})");
                Ok(out)
            }
            None => Ok(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_row_layout_matches_what_the_bindings_promise() {
        // OLE DB writes by byte offset, so a layout change that the bindings
        // do not follow is memory corruption, not a wrong answer.
        let b0 = binding(1, 0);
        let b1 = binding(2, 1);
        assert_eq!(b0.obStatus, 0);
        assert_eq!(b0.obLength, std::mem::offset_of!(BoundText, length));
        assert_eq!(b0.obValue, std::mem::offset_of!(BoundText, value));
        // The second column's offsets are the first's, shifted by one column.
        let stride = std::mem::size_of::<BoundText>();
        assert_eq!(b1.obStatus, b0.obStatus + stride);
        assert_eq!(b1.obLength, b0.obLength + stride);
        assert_eq!(b1.obValue, b0.obValue + stride);
        // Every binding must stay inside the row it describes.
        assert!(b1.obValue + b1.cbMaxLen <= std::mem::size_of::<BoundRow>());
        assert_eq!(std::mem::size_of::<BoundRow>(), 2 * stride);
        assert_eq!(b0.cbMaxLen, TEXT_UNITS * 2);
        assert_eq!(b0.wType, DBTYPE_WSTR.0 as u16);
    }

    #[test]
    fn a_row_reads_back_only_columns_the_provider_marked_ok() {
        let mut row = BoundRow::zeroed();
        let text: Vec<u16> = "C:\\notes.txt".encode_utf16().collect();
        row.columns[0].status = DBSTATUS_S_OK.0 as u32;
        row.columns[0].length = text.len() * 2;
        row.columns[0].value[..text.len()].copy_from_slice(&text);
        // Column 1 left with a non-OK status: no value.
        row.columns[1].status = 3; // DBSTATUS_S_ISNULL
        assert_eq!(row.text(0).as_deref(), Some("C:\\notes.txt"));
        assert_eq!(row.text(1), None);
    }

    #[test]
    fn an_over_long_value_is_truncated_rather_than_read_past_the_buffer() {
        let mut row = BoundRow::zeroed();
        row.columns[0].status = DBSTATUS_S_OK.0 as u32;
        // A provider that reports more than it wrote must not make us read
        // past the array.
        row.columns[0].length = usize::MAX;
        let s = row.text(0).expect("still readable");
        assert_eq!(s.chars().count(), TEXT_UNITS);
    }

    #[test]
    fn empty_queries_short_circuit_before_any_com_work() {
        let s = WindowsSearch;
        assert_eq!(s.search("", 10).unwrap(), Vec::new());
        assert_eq!(s.search("   ", 10).unwrap(), Vec::new());
        assert_eq!(s.search("notes", 0).unwrap(), Vec::new());
    }

    /// The real Windows Search on this machine. It is `#[ignore]`d only
    /// because a runner may have `WSearch` disabled, which is a legitimate
    /// state this code must report rather than crash on — the assertion
    /// below accepts both outcomes, so running it is always meaningful.
    #[test]
    #[ignore = "queries the machine's real Windows Search index; CI runs it with --ignored"]
    fn a_real_query_either_answers_or_says_it_cannot() {
        let s = WindowsSearch;
        // Print the generated SQL: when a query that works in another tool
        // returns nothing here, this is the first thing to look at. The
        // apartment matters — every call below is COM.
        {
            let _sta = Apartment::sta();
            match build_query("notes", 10) {
                Ok((sql, conn)) => {
                    println!("SQL: {sql}");
                    println!("CONN: {conn}");
                }
                Err(e) => println!("build_query failed: {e}"),
            }
        }
        match s.search("notes", 10) {
            Ok(hits) => {
                for h in &hits {
                    assert!(!h.path.is_empty(), "a hit with no path");
                    assert!(!h.name.is_empty(), "a hit with no name");
                }
                println!("windows search returned {} hits", hits.len());
                for h in hits.iter().take(5) {
                    println!("  {} | {}", h.name, h.path);
                }
                assert!(
                    !hits.is_empty(),
                    "the index has content but the read produced nothing"
                );
            }
            // §3.1: an unavailable index is a state to report, not a crash.
            Err(SearchError::Unavailable(m)) => println!("unavailable, as allowed: {m}"),
            Err(e) => panic!("query failed: {e}"),
        }
    }
}
