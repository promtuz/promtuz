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
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.coroutines.Job
import kotlinx.coroutines.CancellationException
import timber.log.Timber
import uniffi.core.GroupRulesRecord

/** A person as the group screens show them: name, key, and their standing. */
data class UiMember(
    val ipkHex: String,
    val name: String,
    /** 0 member, 1 admin, 2 owner. */
    val role: Int = 0,
    val active: Boolean = true,
    val me: Boolean = false,
    /** They told us this name; we didn't choose it. Worth marking as such. */
    val claimed: Boolean = false,
) {
    val admin get() = role >= 1
    val owner get() = role == 2
}

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
                _work.value = GroupWork.Failed(failure)
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

    /** We are an admin or an owner: we may remove members and change the group's rules. */
    private val _canManage = MutableStateFlow(false)
    val canManage: StateFlow<Boolean> = _canManage.asStateFlow()

    /** The group's rules let us add people. */
    private val _canAdd = MutableStateFlow(false)
    val canAdd: StateFlow<Boolean> = _canAdd.asStateFlow()

    /** The group's rules let us rename it and change its photo. */
    private val _canEdit = MutableStateFlow(false)
    val canEdit: StateFlow<Boolean> = _canEdit.asStateFlow()

    /** Our role: 0 member, 1 admin, 2 owner. */
    private val _role = MutableStateFlow(0)
    val role: StateFlow<Int> = _role.asStateFlow()

    /** What members who aren't admins may do. Null for a group from before rules were signed. */
    private val _rules = MutableStateFlow<GroupRulesRecord?>(null)
    val rules: StateFlow<GroupRulesRecord?> = _rules.asStateFlow()

    /** Whose phone makes the group's changes; what anyone else asks for waits for it. */
    private var committerHex: String? = null

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
                        role = if (m.active) m.role.toInt() else 0,
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
                _canAdd.value = record.canAdd
                _canEdit.value = record.canEdit
                _role.value = record.role.toInt()
                _rules.value = record.rules
                committerHex = record.committer?.toHex()
                _canLeave.value = record.canLeave
                _ownerIsStuck.value = record.ownerIsStuck
            }
        }
    }

    /** Everyone picked, in one change. It either adds them all or none. */
    fun addMembers(people: List<UiMember>, onComplete: () -> Unit) {
        if (people.isEmpty()) return
        val names = people.joinToString { it.name }
        perform(if (people.size == 1) "Adding ${people[0].name}…" else "Adding ${people.size} members…",
            "Couldn’t add $names. Try again.") {
            val done = CoreBridge.addGroupMembers(conversation, people.map { it.ipkHex.fromHex() }) ||
                awaitRoster { roster -> people.all { p -> roster.any { it.ipkHex == p.ipkHex } } }
            _notice.value = when {
                !done -> "$names will join when ${committerName()} is next online"
                people.size == 1 -> "Member added"
                else -> "${people.size} members added"
            }
            onComplete()
        }
    }

    fun removeMember(person: UiMember, onComplete: () -> Unit) =
        perform("Removing ${person.name}…", "Couldn’t remove ${person.name}. Try again.") {
            val done = CoreBridge.removeGroupMember(conversation, person.ipkHex.fromHex()) ||
                awaitRoster { roster -> roster.none { it.ipkHex == person.ipkHex } }
            _notice.value = if (done) "${person.name} removed"
                            else "${person.name} will be removed when ${committerName()} is next online"
            onComplete()
        }

    fun setRole(person: UiMember, role: Int) =
        perform("Updating ${person.name}…", "Couldn’t update ${person.name}. Try again.") {
            val done = CoreBridge.setGroupRole(conversation, person.ipkHex.fromHex(), role) ||
                awaitRoster { roster -> roster.any { it.ipkHex == person.ipkHex && it.role == role } }
            _notice.value = when {
                !done -> "This will apply when ${committerName()} is next online"
                role == 2 -> "${person.name} is now an owner"
                role == 1 -> "${person.name} is now an admin"
                else -> "${person.name} is no longer an admin"
            }
        }

    fun setRules(rules: GroupRulesRecord) =
        perform("Saving…", "Couldn’t change the group’s settings. Try again.") {
            val done = CoreBridge.setGroupRules(conversation, rules) ||
                withTimeoutOrNull(12_000) { _rules.first { it == rules } } != null
            if (!done) _notice.value = "This will apply when ${committerName()} is next online"
        }

    /**
     * Only one member's phone changes the group, so a change we asked it for
     * lands when it has run. Usually that's moments; give it those before saying so.
     */
    private suspend fun awaitRoster(done: (List<UiMember>) -> Boolean): Boolean =
        withTimeoutOrNull(12_000) { members.first { roster -> done(roster.filter { it.active }) } } != null

    private fun committerName() = _members.value.firstOrNull { it.ipkHex == committerHex }?.name ?: "an admin"

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
