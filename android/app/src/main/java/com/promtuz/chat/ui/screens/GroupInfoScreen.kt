package com.promtuz.chat.ui.screens

import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.ui.components.TextButton
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.chat.utils.extensions.fromHex
import kotlinx.coroutines.launch
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.presentation.viewmodel.*
import com.promtuz.chat.ui.components.AppBottomSheet
import com.promtuz.chat.ui.components.*
import androidx.compose.ui.graphics.Color
import org.koin.androidx.compose.koinViewModel
import com.promtuz.chat.utils.media.rememberAvatar
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.pictureItem
import com.promtuz.chat.ui.theme.Promtuz

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
    val canAdd by viewModel.canAdd.collectAsStateWithLifecycle()
    val canEdit by viewModel.canEdit.collectAsStateWithLifecycle()
    val role by viewModel.role.collectAsStateWithLifecycle()
    val rules by viewModel.rules.collectAsStateWithLifecycle()
    val candidates by viewModel.candidates.collectAsStateWithLifecycle()
    val work by viewModel.work.collectAsStateWithLifecycle()
    val canLeave by viewModel.canLeave.collectAsStateWithLifecycle()
    val ownerIsStuck by viewModel.ownerIsStuck.collectAsStateWithLifecycle()
    val muted by viewModel.muted.collectAsStateWithLifecycle()
    val notice by viewModel.notice.collectAsStateWithLifecycle()
    LaunchedEffect(conversationHex) { viewModel.load(conversationHex) }
    GroupInfoContent(
        GroupInfoState(members, title, displayName, canManage, candidates, work, canLeave,
            ownerIsStuck, muted, notice, loading, loadError, contactsLoading, contactsError, canAdd,
            canEdit, role, rules?.adminsAppoint == true, rules != null),
        GroupInfoActions(
            clearNotice = viewModel::clearNotice,
            clearError = viewModel::clearError,
            reload = { viewModel.load(conversationHex) },
            loadContacts = viewModel::loadContacts,
            rename = viewModel::rename,
            setMuted = viewModel::setMuted,
            addMembers = viewModel::addMembers,
            removeMember = viewModel::removeMember,
            setRole = viewModel::setRole,
            leave = { viewModel.leave() },
            deleteAnyway = { viewModel.deleteAnyway() },
        ), conversation = conversationHex,
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
    val canAdd: Boolean = false,
    val canEdit: Boolean = false,
    /** Ours: 0 member, 1 admin, 2 owner. */
    val role: Int = 0,
    val adminsAppoint: Boolean = false,
    /** The group's rules are signed into it, so it has settings to open. */
    val hasRules: Boolean = false,
)

internal data class GroupInfoActions(
    val clearNotice: () -> Unit,
    val clearError: () -> Unit,
    val reload: () -> Unit,
    val loadContacts: () -> Unit,
    val rename: (String, () -> Unit) -> Unit,
    val setMuted: (Boolean) -> Unit,
    val addMembers: (List<UiMember>, () -> Unit) -> Unit,
    val removeMember: (UiMember, () -> Unit) -> Unit,
    val setRole: (UiMember, Int) -> Unit,
    val leave: () -> Unit,
    val deleteAnyway: () -> Unit,
)

@Composable
internal fun GroupInfoContent(state: GroupInfoState, actions: GroupInfoActions, conversation: String? = null) = with(state) {
    val app = org.koin.compose.koinInject<AppVM>()
    val scope = rememberCoroutineScope()
    val picker = androidx.activity.compose.rememberLauncherForActivityResult(androidx.activity.result.contract.ActivityResultContracts.PickVisualMedia()) { uri ->
        if (uri != null && conversation != null) app.navigator.push(Routes.ProfilePhoto(uri.toString(), conversation))
    }
    val picture = rememberAvatar(conversation?.let { "group:$it" })
    val media by remember(conversation) {
        com.promtuz.core.observeQuery(setOf("messages", "message_media")) {
            conversation?.let { com.promtuz.core.CoreBridge.sharedMedia(it.fromHex()).size } ?: 0
        }
    }.collectAsState(0)
    val snackbar = remember { SnackbarHostState() }
    LaunchedEffect(notice) { notice?.let { snackbar.showSnackbar(it); actions.clearNotice() } }
    var editing by rememberSaveable { mutableStateOf(false) }
    var draft by rememberSaveable { mutableStateOf("") }
    var adding by rememberSaveable { mutableStateOf(false) }
    var searching by rememberSaveable { mutableStateOf(false) }
    var query by rememberSaveable { mutableStateOf("") }
    var selected by rememberSaveable { mutableStateOf(setOf<String>()) }
    var showPast by rememberSaveable { mutableStateOf(false) }
    var showAll by rememberSaveable { mutableStateOf(false) }
    val presence by com.promtuz.core.CoreBridge.presenceByPeer.collectAsState()
    var removing by remember { mutableStateOf<UiMember?>(null) }
    var leaving by remember { mutableStateOf(false) }
    var deleting by remember { mutableStateOf(false) }
    var removingLock by remember { mutableStateOf(false) }
    var clearing by remember { mutableStateOf(false) }
    val privateIds by app.privateIds.collectAsState()
    val locked = conversation != null && conversation in privateIds
    val busy = work is GroupWork.Busy
    BackHandler(busy) { }
    val active = members.filter { it.active }
    val past = members.filterNot { it.active }
    val addable = candidates.filter { c -> active.none { it.ipkHex == c.ipkHex } }
    val dialogOpen = editing || adding || removing != null || leaving || deleting
    fun actionsFor(member: UiMember): List<MenuAction> = buildList {
        if (member.me || !member.active) return@buildList
        // A group from before signed rules has no roles to hand out.
        val owner = role == 2 && hasRules
        val appoints = owner || (role == 1 && adminsAppoint && !member.owner)
        if (appoints && !member.admin) add(MenuAction("Make admin", icon = R.drawable.oi_user_shield) { actions.setRole(member, 1) })
        if (owner && !member.owner) add(MenuAction("Make owner", icon = R.drawable.oi_user_crown) { actions.setRole(member, 2) })
        if (appoints && member.admin) add(MenuAction(if (member.owner) "Dismiss as owner" else "Dismiss as admin",
            icon = R.drawable.oi_user_arrow_down) { actions.setRole(member, 0) })
        if (canManage && (role == 2 || !member.owner)) add(MenuAction("Remove", icon = R.drawable.oi_user_minus, destructive = true) {
            removing = member; actions.clearError()
        })
    }

    ProfileScaffold(name = displayName.ifBlank { "Group" }, subtitle = if (loading || loadError) null else memberTally(active.size), photo = { size ->
        GroupAvatar(title, active.filterNot { it.me }.map { it.name }, size = size, conversation = conversation,
            onClick = picture?.let { { MediaViewer.open(listOf(pictureItem("group:$conversation", it, displayName))) } })
    }, actions = {
        if (conversation != null) AppDropMenu(
            anchor = { DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp), desc = "Group options") },
            groups = listOf(buildList {
                if (canEdit) add(MenuAction("Edit name", R.drawable.oi_edit) { draft = title; editing = true; actions.clearError() })
                if (canEdit) add(MenuAction("Set group photo", R.drawable.oi_camera) {
                    picker.launch(androidx.activity.result.PickVisualMediaRequest(androidx.activity.result.contract.ActivityResultContracts.PickVisualMedia.ImageOnly))
                })
                if (canEdit && picture != null) add(MenuAction("Remove group photo", R.drawable.oi_trash, destructive = true) {
                    scope.launch {
                        runCatching { com.promtuz.core.CoreBridge.clearGroupPicture(conversation.fromHex()) }
                            .onFailure { snackbar.showSnackbar("Couldn't remove group photo") }
                    }
                })
            }, listOf(
                if (locked) MenuAction("Remove lock", R.drawable.oi_lock_chat_open) { removingLock = true }
                else MenuAction("Lock chat", R.drawable.oi_lock_chat) { app.navigator.push(Routes.LockChat(conversation)) },
            ), buildList {
                if (!locked) add(MenuAction("Clear history", R.drawable.oi_broom) { clearing = true })
            }).filter { it.isNotEmpty() },
        )
    }) { padding, list ->
        Box(Modifier.fillMaxSize()) {
            if (loading || loadError) {
                Column(Modifier.fillMaxWidth().padding(padding).padding(24.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                    if (loading) CircularProgressIndicator()
                    else {
                        Text("Couldn’t load the group")
                        TextButton(onClick = { actions.reload() }) { Text("Retry") }
                    }
                }
            } else LazyColumn(Modifier.fillMaxSize(), list, contentPadding = padding.listPadding(top = 12.dp, bottom = 80.dp)) {
                if (conversation != null) item("actions") {
                    ProfileActions(listOf(
                        MenuAction("Message", R.drawable.i_send) { app.openChat(conversation, displayName) },
                        MenuAction(if (muted) "Unmute" else "Mute",
                            if (muted) R.drawable.i_speaker else R.drawable.i_volume_off) {
                            if (!busy) actions.setMuted(!muted)
                        },
                        MenuAction("Search", R.drawable.i_search) {
                            pendingChatSearch.value = conversation; app.openChat(conversation, displayName)
                        },
                    ))
                }
                if (!dialogOpen) item("work") { GroupWorkFeedback(work) }
                val manage = canManage && hasRules && conversation != null
                if (manage || (media > 0 && conversation != null)) item("settings") {
                    SettingsCard(Modifier.padding(top = 12.dp)) {
                        if (manage) SettingsRow("Group settings", icon = R.drawable.i_settings,
                            subtitle = "Who can send, edit info and add people", trailing = Trailing.Chevron,
                            onClick = { app.navigator.push(Routes.GroupSettings(conversation!!)) })
                        if (media > 0 && conversation != null) SettingsRow("Shared media", icon = R.drawable.i_image,
                            value = media.toString(), trailing = Trailing.Chevron,
                            onClick = { app.navigator.push(Routes.SharedMedia(conversation, displayName)) })
                    }
                }
                val open: (UiMember) -> Unit = { member ->
                    app.navigator.push(if (member.me) Routes.Profile
                        else Routes.PersonInfo(member.ipkHex, member.name))
                }
                val shown = if (showAll || active.size <= MemberPreview + 1) active else active.take(MemberPreview)
                val more = active.size - shown.size
                val rows = 1 + (if (canAdd) 1 else 0) + shown.size + (if (more > 0) 1 else 0) +
                    (if (past.isNotEmpty()) 1 else 0) + (if (showPast) past.size else 0)
                var index = 0
                index++
                item("members-header") {
                    LazyCardHeader(memberTally(active.size), rows, Modifier.padding(top = 12.dp))
                }
                if (canAdd) {
                    val i = index++
                    item("members-add") {
                        Box(Modifier.lazyCardRow(i, rows, dividerStart = 58.dp, divider = i > 1)) {
                            SettingsRow("Add members", icon = R.drawable.i_user_add, titleColor = Promtuz.colors.accent,
                                enabled = !busy, onClick = { adding = true; selected = emptySet(); query = ""; searching = false; actions.clearError() })
                        }
                    }
                }
                val first = index
                itemsIndexed(shown, key = { _, it -> it.ipkHex }) { i, member ->
                    GroupMemberRow(member, first + i, rows, !busy, actionsFor(member), presence[member.ipkHex], onClick = { open(member) })
                }
                index += shown.size
                if (more > 0) {
                    val i = index++
                    item("members-more") {
                        Box(Modifier.lazyCardRow(i, rows, dividerStart = 58.dp)) {
                            SettingsRow("Show $more more", icon = R.drawable.i_users, titleColor = Promtuz.colors.accent,
                                onClick = { showAll = true })
                        }
                    }
                }
                if (past.isNotEmpty()) {
                    val i = index++
                    item("members-past") {
                        Box(Modifier.lazyCardRow(i, rows)) {
                            SettingsRow(if (showPast) "Hide past members" else "Past members", value = if (showPast) null else past.size.toString(),
                                titleColor = Promtuz.colors.accent, onClick = { showPast = !showPast })
                        }
                    }
                    val pastFirst = index
                    if (showPast) itemsIndexed(past, key = { _, it -> "past:${it.ipkHex}" }) { i, member ->
                        GroupMemberRow(member, pastFirst + i, rows, !busy, emptyList(), null, onClick = { open(member) })
                    }
                }
                if (ownerIsStuck) {
                    item("stuck") {
                        Column(Modifier.padding(top = 12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                            Text("You own this group. To leave, first remove the other members.",
                                Modifier.padding(horizontal = 16.dp), style = FootnoteStyle, color = Promtuz.colors.muted)
                            SettingsCard { SettingsRow("Delete chat", titleColor = Promtuz.colors.danger, enabled = !busy,
                                onClick = { deleting = true; actions.clearError() }) }
                        }
                    }
                } else if (canLeave) item("leave") {
                    SettingsCard(Modifier.padding(top = 12.dp)) { SettingsRow("Leave group", titleColor = Promtuz.colors.danger, enabled = !busy,
                        onClick = { leaving = true; actions.clearError() }) }
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
        val sheetState = rememberBottomSheetState(
            initialValue = SheetValue.Hidden,
            enabledValues = setOf(SheetValue.Hidden, SheetValue.Expanded),
            confirmValueChange = { !currentBusy },
        )
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
                SearchableHeader(topBarColors = com.promtuz.chat.ui.theme.transparentTopAppBar(), title = "Add members", selectionCount = selected.size.takeIf { it > 0 },
                    searching = searching, query = query, onQuery = { query = it }, close = true,
                    onBack = ::closePickerMode, onSearch = { searching = true }, enabled = !busy)
                Text("New members won’t see earlier messages.", Modifier.padding(horizontal = 20.dp, vertical = 8.dp),
                    style = RowSubtitleStyle, color = Promtuz.colors.muted)
                if (contactsLoading) LinearProgressIndicator(Modifier.fillMaxWidth())
                if (contactsError) Row(Modifier.padding(horizontal = 16.dp)) {
                    Text("Couldn’t load contacts", Modifier.weight(1f), color = Promtuz.colors.danger)
                    TextButton(onClick = actions.loadContacts) { Text("Retry") }
                }
                ContactPicker(Modifier.weight(1f), addable, query, selected, true, !busy,
                    onClick = { selected = if (it.ipkHex in selected) selected - it.ipkHex else selected + it.ipkHex }, emptyText = when { contactsLoading || contactsError -> ""; candidates.isEmpty() -> "Add contacts before inviting members"; else -> "All your contacts are already in this group" })
                GroupWorkFeedback(work)
                val picks = addable.filter { it.ipkHex in selected }
                AppButton(if (picks.size == 1) "Add 1 member" else "Add ${picks.size} members",
                    onClick = { actions.addMembers(picks) { dismissRequested = true } },
                    enabled = !busy && picks.isNotEmpty(), modifier = Modifier.fillMaxWidth().padding(16.dp))
            }
        }
    }
    removing?.let { member -> GroupConfirmation(
        "Remove ${member.name}?", "They won’t receive new messages from this group.", "Remove", work,
        onDismiss = { removing = null; actions.clearError() },
        onConfirm = { actions.removeMember(member) { removing = null } },
    ) }
    GroupConfirmation("Leave group?", "You won’t receive new messages. Your chat history stays on this device.",
        "Leave", work, { leaving = false; actions.clearError() }, { actions.leave() }, visible = leaving)
    GroupConfirmation("Delete chat?",
        "This deletes your messages and group access from this device. Other members keep the group, but no one will be able to add or remove members. This can’t be undone.",
        "Delete", work, { deleting = false; actions.clearError() }, { actions.deleteAnyway() }, visible = deleting)
    if (clearing && conversation != null) ClearHistoryDialog(displayName,
        onConfirm = { clearing = false; app.clearHistory(conversation) }, onDismiss = { clearing = false })
    if (removingLock && conversation != null) RemoveLockDialog(app, conversation, displayName) { removingLock = false }
}

private const val MemberPreview = 5

@Composable
private fun GroupMemberRow(member: UiMember, index: Int, count: Int, enabled: Boolean, actions: List<MenuAction>,
    presence: Presence?, onClick: () -> Unit) {
    val now = rememberPresenceTime()
    val menu = actions.isNotEmpty() && enabled
    val row = @Composable {
        SettingsRow(
            member.name,
            subtitle = if (!member.active) "Past member" else if (member.me) null else presenceText(presence, now),
            value = if (member.admin && member.active) (if (member.owner) "Owner" else "Admin") else null,
            enabled = enabled,
            leading = {
                Avatar(
                    member.name, size = 44.dp, image = rememberAvatar(member.ipkHex), identityKey = member.ipkHex,
                    originKey = "avatar-${member.ipkHex}", onClick = onClick.takeIf { enabled },
                )
            },
            leadingSize = 44.dp,
            onClick = onClick.takeIf { !menu },
        )
    }
    Box(Modifier.lazyCardRow(index, count, dividerStart = 78.dp, divider = index > 1)) {
        if (!menu) row()
        else AppDropMenu(anchor = row, groups = listOf(actions), onClick = onClick, onClickLabel = "Open profile",
            onLongClickLabel = "Member options")
    }
}

@Composable
private fun GroupConfirmation(title: String, message: String, action: String, work: GroupWork,
    onDismiss: () -> Unit, onConfirm: () -> Unit, visible: Boolean = true) {
    val busy = work is GroupWork.Busy
    if (visible) AppAlertDialog(onDismissRequest = { if (!busy) onDismiss() }, title = { Text(title) },
        text = { Column { Text(message); GroupWorkFeedback(work) } },
        confirmButton = { TextButton(onClick = onConfirm, enabled = !busy) { Text(action, color = MaterialTheme.colorScheme.error) } },
        dismissButton = { TextButton(onClick = onDismiss, enabled = !busy) { Text("Cancel") } })
}
