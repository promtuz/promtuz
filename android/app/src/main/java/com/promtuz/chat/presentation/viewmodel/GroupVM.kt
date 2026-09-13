package com.promtuz.chat.presentation.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.Job
import kotlinx.coroutines.CancellationException
import timber.log.Timber

/** A person as the group screens show them: name, key, and their standing. */
data class UiMember(
    val ipkHex: String,
    val name: String,
    val admin: Boolean = false,
    val active: Boolean = true,
    val me: Boolean = false,
    /** They told us this name; we didn't choose it. Worth marking as such. */
    val claimed: Boolean = false,
)

/** What a membership call is doing right now, so the UI can hold still. */
sealed interface GroupWork {
    data object Idle : GroupWork
    data class Busy(val label: String) : GroupWork
    data class Failed(val reason: String) : GroupWork
}

/**
 * The create flow and the member list.
 *
 * Every membership call needs the network — a KeyPackage fetch and a Welcome —
 * so unlike sending a message these can genuinely fail, and the screen says so
 * rather than optimistically pretending. [work] is what the buttons watch.
 */
class GroupVM(app: AppVM) : ViewModel() {
    // The back stack lives on AppVM, which is the Koin singleton; AppNavigator
    // itself is a property of it, not a definition of its own.
    private val navigator = app.navigator

    private val _work = MutableStateFlow<GroupWork>(GroupWork.Idle)
    val work: StateFlow<GroupWork> = _work.asStateFlow()
    private val _notice = MutableStateFlow<String?>(null)
    val notice = _notice.asStateFlow()
    fun clearNotice() { _notice.value = null }
    private val _muted = MutableStateFlow(false)
    val muted = _muted.asStateFlow()
    private var rosterJob: Job? = null
    private val _loading = MutableStateFlow(true)
    val loading = _loading.asStateFlow()
    private val _loadError = MutableStateFlow(false)
    val loadError = _loadError.asStateFlow()

    private fun perform(label: String, failure: String, block: suspend () -> Unit) {
        if (_work.value is GroupWork.Busy) return
        _work.value = GroupWork.Busy(label)
        viewModelScope.launch {
            try {
                block()
                _work.value = GroupWork.Idle
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Timber.tag(TAG).e(e, label)
                _work.value = GroupWork.Failed(
                    if (e is MemberAddFailure) {
                        val prefix = if (e.added > 0) "${e.added} added. " else ""
                        prefix + "Couldn’t add ${e.names.joinToString()}. Try again."
                    } else failure
                )
            }
        }
    }

    // — Create flow —

    private val _title = MutableStateFlow("")
    val title: StateFlow<String> = _title.asStateFlow()

    private val _picked = MutableStateFlow<Set<String>>(emptySet())
    val picked: StateFlow<Set<String>> = _picked.asStateFlow()

    /** Address book used by the shared contact picker. */
    private val _candidates = MutableStateFlow<List<UiMember>>(emptyList())
    val candidates: StateFlow<List<UiMember>> = _candidates.asStateFlow()

    private val _contactsLoading = MutableStateFlow(true)
    val contactsLoading = _contactsLoading.asStateFlow()
    private val _contactsError = MutableStateFlow(false)
    val contactsError = _contactsError.asStateFlow()
    private var contactsJob: Job? = null

    init { loadContacts() }

    fun loadContacts() {
        contactsJob?.cancel()
        _contactsLoading.value = true
        contactsJob = viewModelScope.launch {
            observeQuery(setOf("contacts")) {
                try {
                    val people = CoreBridge.contacts().map { UiMember(it.ipk.toHex(), it.name) }
                        .sortedBy { it.name.lowercase() }
                    _contactsError.value = false
                    people
                } catch (e: CancellationException) { throw e }
                catch (e: Exception) {
                    Timber.tag(TAG).e(e, "load contacts failed")
                    _contactsError.value = true
                    _candidates.value
                }
            }.collect {
                _candidates.value = it
                _picked.value = _picked.value.intersect(it.map { person -> person.ipkHex }.toSet())
                _contactsLoading.value = false
            }
        }
    }

    fun setTitle(value: String) { _title.value = value }

    fun togglePick(ipkHex: String) {
        _picked.value = if (ipkHex in _picked.value) _picked.value - ipkHex
                        else _picked.value + ipkHex
    }

    fun create(onCreated: () -> Unit = {}) {
        val people = _candidates.value.filter { it.ipkHex in _picked.value }.map { it.ipkHex }
        val name = _title.value.trim()
        if (people.isEmpty() || name.isEmpty() || name.codePointCount(0, name.length) > 64) return
        perform("Creating group…", "Couldn’t create the group. Try again.") {
            val conv = CoreBridge.createGroup(name, people.map { it.fromHex() })
            onCreated()
            navigator.back()
            navigator.push(Routes.Chat(conv.toHex(), name))
        }
    }

    fun clearPicks() { _picked.value = emptySet() }

    // — Member list —

    private val _members = MutableStateFlow<List<UiMember>>(emptyList())
    val members: StateFlow<List<UiMember>> = _members.asStateFlow()

    /** The name actually set, blank until someone sets one — what rename edits. */
    private val _groupTitle = MutableStateFlow("")
    val groupTitle: StateFlow<String> = _groupTitle.asStateFlow()

    /** What to head the screen with; falls back to the members for an unnamed group. */
    private val _displayName = MutableStateFlow("")
    val displayName: StateFlow<String> = _displayName.asStateFlow()

    /** True when we may add and remove — v1 grants that to the creator alone. */
    private val _canManage = MutableStateFlow(false)
    val canManage: StateFlow<Boolean> = _canManage.asStateFlow()

    /** Leaving is offered: we are in the group and wouldn't strand it. */
    private val _canLeave = MutableStateFlow(false)
    val canLeave: StateFlow<Boolean> = _canLeave.asStateFlow()

    /**
     * We founded this group and others are still here, so leaving is refused —
     * it would leave everyone in a group nobody can manage.
     */
    private val _ownerIsStuck = MutableStateFlow(false)
    val ownerIsStuck: StateFlow<Boolean> = _ownerIsStuck.asStateFlow()

    private var conversation: ByteArray = ByteArray(16)

    fun load(conversationHex: String) {
        rosterJob?.cancel()
        conversation = conversationHex.fromHex()
        _loading.value = true
        rosterJob = viewModelScope.launch {
            observeQuery(setOf("conversations", "conversation_members", "contacts")) {
                try {
                    val record = CoreBridge.conversation(conversation) ?: error("Group not found")
                    val roster = CoreBridge.members(conversation)
                    _loadError.value = false
                    Pair(record, roster)
                } catch (e: CancellationException) { throw e }
                catch (e: Exception) {
                    Timber.tag(TAG).e(e, "load group failed")
                    _loadError.value = true
                    null
                }
            }.collect { result ->
                _loading.value = false
                if (result == null) return@collect
                val (record, roster) = result
                _muted.value = record.muted
                _groupTitle.value = record.title
                _displayName.value = record.displayName
                // Core resolves the name and whether it is theirs to assert;
                // only "You" is ours to say.
                _members.value = roster.map { m ->
                    UiMember(
                        ipkHex = m.ipk.toHex(),
                        name = if (m.me) "You" else m.name,
                        claimed = !m.me && m.nameIsClaimed,
                        admin = m.role.toInt() == 1,
                        active = m.active,
                        me = m.me,
                    )
                }.sortedWith(
                    // Us first, then everyone still here, then by name.
                    compareByDescending<UiMember> { it.me }
                        .thenByDescending { it.active }
                        .thenBy { it.name.lowercase() },
                )
                _canManage.value = record.canManage
                _canLeave.value = record.canLeave
                _ownerIsStuck.value = record.ownerIsStuck
            }
        }
    }

    fun addMembers(people: List<UiMember>, onAdded: (String) -> Unit, onComplete: () -> Unit) {
        if (people.isEmpty()) return
        perform("Adding members…", "Couldn’t finish adding members. Try again.") {
            val failed = mutableListOf<String>()
            for (person in people) {
                _work.value = GroupWork.Busy("Adding ${person.name}…")
                try {
                    CoreBridge.addGroupMember(conversation, person.ipkHex.fromHex())
                    onAdded(person.ipkHex)
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    Timber.tag(TAG).e(e, "add member failed")
                    failed += person.name
                }
            }
            val added = people.size - failed.size
            if (failed.isEmpty()) {
                _notice.value = if (added == 1) "Member added" else "$added members added"
                onComplete()
            } else {
                // Keep only unsuccessful selections so retry cannot add someone twice.
                throw MemberAddFailure(added, failed)
            }
        }
    }

    private class MemberAddFailure(val added: Int, val names: List<String>) : Exception()

    fun removeMember(person: UiMember, onComplete: () -> Unit) =
        perform("Removing ${person.name}…", "Couldn’t remove ${person.name}. Try again.") {
            CoreBridge.removeGroupMember(conversation, person.ipkHex.fromHex())
            _notice.value = "${person.name} removed"
            onComplete()
        }

    fun rename(value: String, onComplete: () -> Unit) {
        val name = value.trim()
        if (name.isEmpty() || name.codePointCount(0, name.length) > 64) return
        perform("Saving name…", "Couldn’t save the name. Try again.") {
            CoreBridge.setConversationTitle(conversation, name)
            _notice.value = "Group name saved"
            onComplete()
        }
    }

    fun setMuted(value: Boolean) =
        perform("Updating notifications…", "Couldn’t update notifications. Try again.") {
            CoreBridge.setConversationMuted(conversation, value)
            _muted.value = value
        }

    fun leave() = perform("Leaving group…", "Couldn’t leave the group. Try again.") {
        CoreBridge.leaveGroup(conversation)
        navigator.reset(Routes.App)
    }

    fun deleteAnyway() = perform("Deleting chat…", "Couldn’t delete the chat. Try again.") {
        CoreBridge.deleteConversation(conversation, force = true)
        navigator.reset(Routes.App)
    }

    fun clearError() { if (_work.value !is GroupWork.Busy) _work.value = GroupWork.Idle }

    private companion object { const val TAG = "GroupVM" }
}
