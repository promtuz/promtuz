package com.promtuz.chat.ui.screens


import androidx.activity.compose.BackHandler
import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.animation.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.res.painterResource
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.stage.ChatMotion
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.presentation.viewmodel.*
import org.koin.androidx.compose.koinViewModel

@Composable
fun ContactsScreen(
    onScanned: (ByteArray) -> Unit,
    onShareIdentity: () -> Unit,
    viewModel: ContactsVM = koinViewModel(),
    group: GroupVM = koinViewModel(),
) {
    val loading by group.contactsLoading.collectAsStateWithLifecycle()
    val loadError by group.contactsError.collectAsStateWithLifecycle()
    val people by group.candidates.collectAsStateWithLifecycle()
    val picked by group.picked.collectAsStateWithLifecycle()
    val title by group.title.collectAsStateWithLifecycle()
    val work by group.work.collectAsStateWithLifecycle()
    val opening by viewModel.busy.collectAsStateWithLifecycle()
    val error by viewModel.error.collectAsStateWithLifecycle()
    val presence by viewModel.presence.collectAsStateWithLifecycle()
    ContactsContent(
        ContactsState(loading, loadError, people, picked, title, work, opening, error, presence),
        ContactsActions(group::clearPicks, group::clearError, group::togglePick, group::loadContacts,
            viewModel::open, viewModel::clearError, viewModel::delete, group::create, group::setTitle),
        onScanned, onShareIdentity,
    )
}

internal data class ContactsState(
    val loading: Boolean = false,
    val loadError: Boolean = false,
    val people: List<UiMember> = emptyList(),
    val picked: Set<String> = emptySet(),
    val title: String = "",
    val work: GroupWork = GroupWork.Idle,
    val opening: Boolean = false,
    val error: String? = null,
    val presence: Map<String, Presence> = emptyMap(),
)

internal data class ContactsActions(
    val clearPicks: () -> Unit,
    val clearGroupError: () -> Unit,
    val togglePick: (String) -> Unit,
    val loadContacts: () -> Unit,
    val open: (UiMember) -> Unit,
    val clearContactError: () -> Unit,
    val delete: (UiMember, () -> Unit) -> Unit,
    val create: (() -> Unit) -> Unit,
    val setTitle: (String) -> Unit,
)

@Composable
internal fun ContactsContent(state: ContactsState, actions: ContactsActions,
    onScanned: (ByteArray) -> Unit, onShareIdentity: () -> Unit) = with(state) {
    val direction = LocalLayoutDirection.current
    val presenceNow = rememberPresenceTime()
    val backDispatcher = LocalOnBackPressedDispatcherOwner.current?.onBackPressedDispatcher
    // TODO: Reintroduce Contacts search with a dedicated UI and interaction design.
    var selecting by rememberSaveable { mutableStateOf(false) }
    var naming by rememberSaveable { mutableStateOf(false) }
    var scanning by remember { mutableStateOf(false) }
    var deleting by remember { mutableStateOf<UiMember?>(null) }
    val busy = work is GroupWork.Busy || opening
    val selectedPeople = people.filter { it.ipkHex in picked }
    fun cancelSelection() { selecting = false; actions.clearPicks(); actions.clearGroupError() }
    fun toggle(person: UiMember) {
        val last = person.ipkHex in picked && picked.size == 1
        actions.togglePick(person.ipkHex)
        selecting = !last
    }
    fun back() {
        if (busy) return
        when {
            selecting -> cancelSelection()
            else -> backDispatcher?.onBackPressed()
        }
    }
    var previousCount by remember { mutableIntStateOf(0) }
    LaunchedEffect(selectedPeople.size) {
        if (previousCount > 0 && selectedPeople.isEmpty()) cancelSelection()
        previousCount = selectedPeople.size
    }
    BackHandler(busy || selecting) { back() }

    ScreenScaffold(topBar = { scrollBehavior ->
        ContactPickerHeader(
            scrollBehavior = scrollBehavior,
            windowInsets = androidx.compose.material3.TopAppBarDefaults.windowInsets,
            title = if (selecting && selectedPeople.isEmpty()) "Select contacts" else "Contacts",
            selectionCount = selectedPeople.size.takeIf { selecting && it > 0 },
            close = selecting, onBack = ::back, enabled = !busy,
            actions = { if (!selecting) IconButton(onClick = onShareIdentity) {
                Icon(painterResource(R.drawable.oi_qr_code), "My QR code", Modifier.size(22.dp))
            } else if (selectedPeople.size == 1) IconButton(
                onClick = { deleting = selectedPeople.single(); actions.clearContactError() }, enabled = !busy,
            ) { Icon(painterResource(R.drawable.i_delete), "Delete contact", Modifier.size(22.dp)) } },
        )
    }) { padding ->
        Box(Modifier.fillMaxSize().padding(
            start = padding.calculateLeftPadding(direction),
            end = padding.calculateRightPadding(direction),
            top = 0.dp,
            bottom = 0.dp
        )) {
            Column(Modifier.fillMaxSize()) {
                ContactPicker(Modifier.weight(1f),
                    people, "", picked, selecting, !busy,
                    onClick = { if (selecting) toggle(it) else actions.open(it) },
                    onLongClick = ::toggle,
                    yPadding = Pair(padding.calculateTopPadding() + 4.dp, padding.calculateBottomPadding() + 88.dp), emptyText = if (loading || loadError) "" else "No contacts yet",
                    supportingText = { person ->
                        val status = presence[person.ipkHex]
                        presenceText(status, presenceNow)?.let { label ->
                            Text(label, style = MaterialTheme.typography.bodySmall,
                                color = if (status == Presence.Online) MaterialTheme.colorScheme.primary
                                    else MaterialTheme.colorScheme.onSurfaceVariant,
                                maxLines = 1, overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis)
                        }
                    },
                    header = {
                        if (opening || loading) LinearProgressIndicator(Modifier.fillMaxWidth())
                        error?.let { Text(it, Modifier.padding(16.dp), color = MaterialTheme.colorScheme.error) }
                        if (loadError) Row(Modifier.padding(horizontal = 16.dp)) {
                            Text("Couldn’t load contacts", Modifier.weight(1f), color = MaterialTheme.colorScheme.error)
                            TextButton(onClick = actions.loadContacts) { Text("Retry") }
                        }
                        AnimatedVisibility(!selecting,
                            enter = fadeIn(ChatMotion.spec()) + expandVertically(ChatMotion.spec()),
                            exit = fadeOut(ChatMotion.spec()) + shrinkVertically(ChatMotion.spec())) {
                            Column(Modifier.padding(horizontal = 18.dp, vertical = 12.dp),
                                verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                GroupedActionRow("Create group", 0, 2,
                                    onClick = { selecting = true }, enabled = !busy && !selecting && people.isNotEmpty()) {
                                    Icon(painterResource(R.drawable.i_users), null, Modifier.size(26.dp))
                                }
                                GroupedActionRow("Add Contact", 1, 2,
                                    onClick = { scanning = true }, enabled = !busy && !selecting) {
                                    Icon(painterResource(R.drawable.i_user_add), null, Modifier.size(26.dp))
                                }
                            }
                        }
                    })
            }
            AnimatedVisibility(selecting && selectedPeople.isNotEmpty(),
                modifier = Modifier.align(Alignment.BottomEnd).padding(bottom = padding.calculateBottomPadding()).padding(16.dp),
                enter = fadeIn(ChatMotion.spec()) + slideInVertically(ChatMotion.spec()) { it },
                exit = fadeOut(ChatMotion.spec()) + slideOutVertically(ChatMotion.spec()) { it }) {
                GroupActionButton("Create group", { naming = true; actions.clearGroupError() }, !busy)
            }
        }
    }
    if (scanning) QrScannerSheet(onResult = { scanning = false; onScanned(it) }, onDismiss = { scanning = false })
    GroupNameDialog(visible = naming, heading = "Create group", value = title, onValueChange = actions.setTitle,
        work = work, confirmLabel = "Create", onConfirm = { actions.create { naming = false } },
        onDismiss = { naming = false; actions.clearGroupError() }, summary = selectedPeople.joinToString { it.name })
    deleting?.let { person -> AppAlertDialog(
        onDismissRequest = { if (!busy) deleting = null },
        title = { Text("Delete ${person.name}?") },
        text = { Column {
            Text("This deletes the contact and your chat history from this device. This can’t be undone.")
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            if (opening || loading) LinearProgressIndicator(Modifier.fillMaxWidth().padding(top = 12.dp))
        } },
        confirmButton = { TextButton(onClick = { actions.delete(person) { deleting = null; cancelSelection() } }, enabled = !busy) {
            Text("Delete", color = MaterialTheme.colorScheme.error)
        } },
        dismissButton = { TextButton(onClick = { deleting = null }, enabled = !busy) { Text("Cancel") } },
    ) }
}