package com.promtuz.chat.ui.screens

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.presentation.viewmodel.*
import com.promtuz.chat.ui.components.AppBottomSheet
import com.promtuz.chat.ui.components.*
import org.koin.androidx.compose.koinViewModel
import com.promtuz.chat.utils.media.rememberAvatar
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.pictureItem

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun GroupInfoScreen(conversationHex: String, viewModel: GroupVM = koinViewModel()) {
    val loading by viewModel.loading.collectAsStateWithLifecycle()
    val loadError by viewModel.loadError.collectAsStateWithLifecycle()
    val contactsLoading by viewModel.contactsLoading.collectAsStateWithLifecycle()
    val contactsError by viewModel.contactsError.collectAsStateWithLifecycle()
    val members by viewModel.members.collectAsStateWithLifecycle()
    val title by viewModel.groupTitle.collectAsStateWithLifecycle()
    val displayName by viewModel.displayName.collectAsStateWithLifecycle()
    val canManage by viewModel.canManage.collectAsStateWithLifecycle()
    val candidates by viewModel.candidates.collectAsStateWithLifecycle()
    val work by viewModel.work.collectAsStateWithLifecycle()
    val canLeave by viewModel.canLeave.collectAsStateWithLifecycle()
    val ownerIsStuck by viewModel.ownerIsStuck.collectAsStateWithLifecycle()
    val muted by viewModel.muted.collectAsStateWithLifecycle()
    val notice by viewModel.notice.collectAsStateWithLifecycle()
    LaunchedEffect(conversationHex) { viewModel.load(conversationHex) }
    GroupInfoContent(
        GroupInfoState(members, title, displayName, canManage, candidates, work, canLeave,
            ownerIsStuck, muted, notice, loading, loadError, contactsLoading, contactsError),
        GroupInfoActions(
            clearNotice = viewModel::clearNotice,
            clearError = viewModel::clearError,
            reload = { viewModel.load(conversationHex) },
            loadContacts = viewModel::loadContacts,
            rename = viewModel::rename,
            setMuted = viewModel::setMuted,
            addMembers = viewModel::addMembers,
            removeMember = viewModel::removeMember,
            leave = { viewModel.leave() },
            deleteAnyway = { viewModel.deleteAnyway() },
        ),
    )
}

internal data class GroupInfoState(
    val members: List<UiMember>,
    val title: String,
    val displayName: String,
    val canManage: Boolean,
    val candidates: List<UiMember>,
    val work: GroupWork = GroupWork.Idle,
    val canLeave: Boolean = false,
    val ownerIsStuck: Boolean = false,
    val muted: Boolean = false,
    val notice: String? = null,
    val loading: Boolean = false,
    val loadError: Boolean = false,
    val contactsLoading: Boolean = false,
    val contactsError: Boolean = false,
)

internal data class GroupInfoActions(
    val clearNotice: () -> Unit,
    val clearError: () -> Unit,
    val reload: () -> Unit,
    val loadContacts: () -> Unit,
    val rename: (String, () -> Unit) -> Unit,
    val setMuted: (Boolean) -> Unit,
    val addMembers: (List<UiMember>, (String) -> Unit, () -> Unit) -> Unit,
    val removeMember: (UiMember, () -> Unit) -> Unit,
    val leave: () -> Unit,
    val deleteAnyway: () -> Unit,
)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun GroupInfoContent(state: GroupInfoState, actions: GroupInfoActions) = with(state) {
    val snackbar = remember { SnackbarHostState() }
    LaunchedEffect(notice) { notice?.let { snackbar.showSnackbar(it); actions.clearNotice() } }
    var editing by rememberSaveable { mutableStateOf(false) }
    var draft by rememberSaveable { mutableStateOf("") }
    var adding by rememberSaveable { mutableStateOf(false) }
    var searching by rememberSaveable { mutableStateOf(false) }
    var query by rememberSaveable { mutableStateOf("") }
    var selected by remember { mutableStateOf(setOf<String>()) }
    var showPast by rememberSaveable { mutableStateOf(false) }
    var person by remember { mutableStateOf<UiMember?>(null) }
    var removing by remember { mutableStateOf<UiMember?>(null) }
    var leaving by remember { mutableStateOf(false) }
    var deleting by remember { mutableStateOf(false) }
    val busy = work is GroupWork.Busy
    BackHandler(busy) { }
    val active = members.filter { it.active }
    val past = members.filterNot { it.active }
    val addable = candidates.filter { c -> active.none { it.ipkHex == c.ipkHex } }
    val dialogOpen = editing || adding || removing != null || leaving || deleting
    val direction = LocalLayoutDirection.current
    val colors = MaterialTheme.colorScheme

    SimpleScreen({ Text("Group info") }) { padding ->
        Box(Modifier.fillMaxSize()) {
            if (loading || loadError) {
                Column(Modifier.fillMaxWidth().padding(padding).padding(24.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                    if (loading) CircularProgressIndicator()
                    else {
                        Text("Couldn’t load the group")
                        TextButton(onClick = { actions.reload() }) { Text("Retry") }
                    }
                }
            } else LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(start = padding.calculateStartPadding(direction), end = padding.calculateEndPadding(direction),
                top = padding.calculateTopPadding(), bottom = padding.calculateBottomPadding() + 80.dp)) {
                item {
                    Column(Modifier.fillMaxWidth().padding(24.dp), horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        GroupAvatar(title, active.filterNot { it.me }.map { it.name }, size = 88.dp)
                        Text(displayName.ifBlank { "Group" }, style = MaterialTheme.typography.headlineSmall)
                        Text(memberTally(active.size), color = colors.onSurfaceVariant)
                        if (canManage) TextButton(onClick = { draft = title; editing = true; actions.clearError() }, enabled = !busy) {
                            Text("Edit group name")
                        }
                    }
                }
                if (!dialogOpen) item { GroupWorkFeedback(work) }
                item {
                    ListItem(
                        headlineContent = { Text("Notifications") },
                        supportingContent = { Text(if (muted) "Muted" else "On") },
                        trailingContent = { Switch(checked = !muted, onCheckedChange = { actions.setMuted(!it) }, enabled = !busy) },
                    )
                    HorizontalDivider(Modifier.padding(vertical = 12.dp))
                    Row(Modifier.fillMaxWidth().padding(horizontal = 20.dp), verticalAlignment = Alignment.CenterVertically) {
                        Text("Members", Modifier.weight(1f), style = MaterialTheme.typography.titleSmall)
                        if (canManage) TextButton(onClick = { adding = true; selected = emptySet(); query = ""; searching = false; actions.clearError() }, enabled = !busy) {
                            Text("Add members")
                        }
                    }
                }
                items(active, key = { it.ipkHex }) { member ->
                    GroupMemberRow(member, !busy) { person = member }
                }
                if (past.isNotEmpty()) {
                    item { TextButton(onClick = { showPast = !showPast }, modifier = Modifier.padding(horizontal = 12.dp)) {
                        Text(if (showPast) "Hide past members" else "Past members (${past.size})")
                    } }
                    if (showPast) items(past, key = { "past:${it.ipkHex}" }) { member ->
                        GroupMemberRow(member, !busy) { person = member }
                    }
                }
                item {
                    HorizontalDivider(Modifier.padding(vertical = 12.dp))
                    if (ownerIsStuck) {
                        Text("You’re the only admin. To leave, first remove the other members.",
                            Modifier.padding(horizontal = 20.dp, vertical = 8.dp), color = colors.onSurfaceVariant)
                        TextButton(onClick = { deleting = true; actions.clearError() }, enabled = !busy,
                            modifier = Modifier.padding(horizontal = 12.dp)) { Text("Delete chat", color = colors.error) }
                    } else if (canLeave) {
                        TextButton(onClick = { leaving = true; actions.clearError() }, enabled = !busy,
                            modifier = Modifier.padding(horizontal = 12.dp)) { Text("Leave group", color = colors.error) }
                    }
                }
            }
            SnackbarHost(snackbar, Modifier.align(Alignment.BottomCenter))
        }
    }
    GroupNameDialog(visible = editing, heading = "Edit group name", value = draft, onValueChange = { draft = it },
        work = work, confirmLabel = "Save", onConfirm = { actions.rename(draft) { editing = false } },
        onDismiss = { editing = false; actions.clearError() }, changed = draft.trim() != title)
    if (adding) {
        var dismissRequested by remember { mutableStateOf(false) }
        val currentBusy by rememberUpdatedState(busy)
        val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true,
            confirmValueChange = { !currentBusy })
        LaunchedEffect(dismissRequested, busy) {
            if (dismissRequested && !busy) {
                sheetState.hide()
                adding = false
                actions.clearError()
            }
        }
        AppBottomSheet(
            onDismissRequest = { if (!busy) { adding = false; actions.clearError() } },
            sheetState = sheetState,
            dismissEnabled = !busy,
        ) {
            Column(Modifier.fillMaxHeight(0.9f)) {
                fun closePickerMode() {
                    if (busy) return
                    if (searching) { searching = false; query = "" }
                    else { dismissRequested = true }
                }
                BackHandler(searching) { closePickerMode() }
                ContactPickerHeader(topBarColors = TopAppBarDefaults.topAppBarColors(
                    containerColor = BottomSheetDefaults.ContainerColor,
                    scrolledContainerColor = BottomSheetDefaults.ContainerColor), title = "Add members", selectionCount = selected.size.takeIf { it > 0 },
                    searching = searching, query = query, onQuery = { query = it }, close = true,
                    onBack = ::closePickerMode, onSearch = { searching = true }, enabled = !busy)
                Text("New members won’t see earlier messages.", Modifier.padding(horizontal = 20.dp, vertical = 8.dp),
                    style = MaterialTheme.typography.bodyMedium, color = colors.onSurfaceVariant)
                if (contactsLoading) LinearProgressIndicator(Modifier.fillMaxWidth())
                if (contactsError) Row(Modifier.padding(horizontal = 16.dp)) {
                    Text("Couldn’t load contacts", Modifier.weight(1f), color = colors.error)
                    TextButton(onClick = actions.loadContacts) { Text("Retry") }
                }
                ContactPicker(Modifier.weight(1f), addable, query, selected, true, !busy,
                    onClick = {
                        val last = it.ipkHex in selected && selected.size == 1
                        selected = if (it.ipkHex in selected) selected - it.ipkHex else selected + it.ipkHex
                        if (last) { dismissRequested = true }
                    }, emptyText = when { contactsLoading || contactsError -> ""; candidates.isEmpty() -> "Add contacts before inviting members"; else -> "All your contacts are already in this group" })
                GroupWorkFeedback(work)
                val picks = addable.filter { it.ipkHex in selected }
                GroupActionButton(if (picks.size == 1) "Add 1 member" else "Add ${picks.size} members",
                    onClick = { actions.addMembers(picks, { selected = selected - it }, { dismissRequested = true }) },
                    enabled = !busy && picks.isNotEmpty(), modifier = Modifier.fillMaxWidth().padding(16.dp))
            }
        }
    }
    person?.let { member -> GroupDialog(
        onDismissRequest = { person = null }, title = { Text(member.name) },
        text = { Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(when { !member.active -> "Past member"; member.admin -> "Group admin"; else -> "Group member" })
            if (member.claimed) Text("This name was set by the member.", color = colors.onSurfaceVariant)
        } },
        confirmButton = {
            if (canManage && member.active && !member.admin && !member.me) TextButton(onClick = {
                removing = member; person = null; actions.clearError()
            }) { Text("Remove from group", color = colors.error) }
            else TextButton(onClick = { person = null }) { Text("Done") }
        },
        dismissButton = { if (canManage && member.active && !member.admin && !member.me) TextButton(onClick = { person = null }) { Text("Cancel") } },
    ) }
    removing?.let { member -> GroupConfirmation(
        "Remove ${member.name}?", "They won’t receive new messages from this group.", "Remove", work,
        onDismiss = { removing = null; actions.clearError() },
        onConfirm = { actions.removeMember(member) { removing = null } },
    ) }
    GroupConfirmation("Leave group?", "You won’t receive new messages. Your chat history stays on this device.",
        "Leave", work, { leaving = false; actions.clearError() }, { actions.leave() }, visible = leaving)
    GroupConfirmation("Delete chat?",
        "This deletes your messages and group access from this device. Other members keep the group, but no one will be able to manage it. This can’t be undone.",
        "Delete", work, { deleting = false; actions.clearError() }, { actions.deleteAnyway() }, visible = deleting)
}

@Composable
private fun GroupMemberRow(member: UiMember, enabled: Boolean, onClick: () -> Unit) {
    ListItem(
        headlineContent = { Text(member.name, maxLines = 1, overflow = TextOverflow.Ellipsis) },
        leadingContent = {
            val avatar = rememberAvatar(member.ipkHex)
            Avatar(
                member.name, size = 44.dp, image = avatar, originKey = "avatar-${member.ipkHex}",
                onClick = avatar?.let { { MediaViewer.open(listOf(pictureItem("avatar-${member.ipkHex}", it, member.name))) } },
            )
        },
        supportingContent = if (!member.active) {{ Text("Past member") }} else null,
        trailingContent = if (member.admin && member.active) {{ Text("Admin", color = MaterialTheme.colorScheme.onSurfaceVariant) }} else null,
        modifier = Modifier.clickable(enabled = enabled, onClick = onClick),
    )
}

@Composable
private fun GroupConfirmation(title: String, message: String, action: String, work: GroupWork,
    onDismiss: () -> Unit, onConfirm: () -> Unit, visible: Boolean = true) {
    val busy = work is GroupWork.Busy
    GroupDialog(visible = visible, onDismissRequest = { if (!busy) onDismiss() }, title = { Text(title) },
        text = { Column { Text(message); GroupWorkFeedback(work) } },
        confirmButton = { TextButton(onClick = onConfirm, enabled = !busy) { Text(action, color = MaterialTheme.colorScheme.error) } },
        dismissButton = { TextButton(onClick = onDismiss, enabled = !busy) { Text("Cancel") } })
}
