package com.promtuz.chat.presentation.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.reason
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import timber.log.Timber

/** A person as the group screens show them: name, key, and their standing. */
data class UiMember(
    val ipkHex: String,
    val name: String,
    val admin: Boolean = false,
    val active: Boolean = true,
    val me: Boolean = false,
)

/** What a membership call is doing right now, so the UI can hold still. */
sealed interface GroupWork {
    data object Idle : GroupWork
    data object Busy : GroupWork
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

    // — Create flow —

    private val _title = MutableStateFlow("")
    val title: StateFlow<String> = _title.asStateFlow()

    private val _picked = MutableStateFlow<Set<String>>(emptySet())
    val picked: StateFlow<Set<String>> = _picked.asStateFlow()

    /** Contacts eligible to be added: paired, so they have a KeyPackage to fetch. */
    private val _candidates = MutableStateFlow<List<UiMember>>(emptyList())
    val candidates: StateFlow<List<UiMember>> = _candidates.asStateFlow()

    init {
        viewModelScope.launch {
            observeQuery(setOf("contacts")) {
                runCatching { CoreBridge.contacts() }.getOrDefault(emptyList())
                    .map { UiMember(it.ipk.toHex(), it.name) }
                    .sortedBy { it.name.lowercase() }
            }.collect { _candidates.value = it }
        }
    }

    fun setTitle(value: String) { _title.value = value }

    fun togglePick(ipkHex: String) {
        _picked.value = if (ipkHex in _picked.value) _picked.value - ipkHex
                        else _picked.value + ipkHex
    }

    /** Create the group and land the user straight in it. */
    fun create() = viewModelScope.launch {
        val members = _picked.value.toList()
        if (members.isEmpty()) return@launch
        _work.value = GroupWork.Busy
        val name = _title.value.trim().ifEmpty { "New group" }
        runCatching { CoreBridge.createGroup(name, members.map { it.fromHex() }) }
            .onSuccess { conv ->
                _work.value = GroupWork.Idle
                _picked.value = emptySet()
                _title.value = ""
                // Drop the setup form off the stack first, so backing out of
                // the new group lands on the chat list rather than the form.
                navigator.back()
                navigator.push(Routes.Chat(conv.toHex(), name))
            }
            .onFailure {
                Timber.tag(TAG).e(it, "create group failed")
                _work.value = GroupWork.Failed(it.reason("Could not create the group"))
            }
    }

    // — Member list —

    private val _members = MutableStateFlow<List<UiMember>>(emptyList())
    val members: StateFlow<List<UiMember>> = _members.asStateFlow()

    private val _groupTitle = MutableStateFlow("")
    val groupTitle: StateFlow<String> = _groupTitle.asStateFlow()

    /** True when we may add and remove — v1 grants that to the creator alone. */
    private val _canManage = MutableStateFlow(false)
    val canManage: StateFlow<Boolean> = _canManage.asStateFlow()

    private var conversation: ByteArray = ByteArray(16)

    fun load(conversationHex: String) {
        conversation = conversationHex.fromHex()
        viewModelScope.launch {
            observeQuery(setOf("conversations", "conversation_members", "contacts")) {
                val record = runCatching { CoreBridge.conversation(conversation) }.getOrNull()
                val roster = runCatching { CoreBridge.members(conversation) }.getOrDefault(emptyList())
                val names = runCatching { CoreBridge.contacts() }.getOrDefault(emptyList())
                    .associate { it.ipk.toHex() to it.name }
                Triple(record, roster, names)
            }.collect { (record, roster, names) ->
                _groupTitle.value = record?.title.orEmpty()
                // Whoever isn't in our address book is still a member — they
                // just have no name yet, so show the key's head rather than
                // dropping them from the roster. We are never in our own
                // address book, so name that row ourselves.
                _members.value = roster.map { m ->
                    val hex = m.ipk.toHex()
                    UiMember(
                        ipkHex = hex,
                        name = if (m.me) "You" else names[hex] ?: hex.take(8),
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
                _canManage.value = record?.canManage == true
            }
        }
    }

    fun addMember(ipkHex: String) = viewModelScope.launch {
        _work.value = GroupWork.Busy
        runCatching { CoreBridge.addGroupMember(conversation, ipkHex.fromHex()) }
            .onSuccess { _work.value = GroupWork.Idle }
            .onFailure {
                Timber.tag(TAG).e(it, "add member failed")
                _work.value = GroupWork.Failed(it.reason("Could not add them"))
            }
    }

    fun removeMember(ipkHex: String) = viewModelScope.launch {
        _work.value = GroupWork.Busy
        runCatching { CoreBridge.removeGroupMember(conversation, ipkHex.fromHex()) }
            .onSuccess { _work.value = GroupWork.Idle }
            .onFailure {
                Timber.tag(TAG).e(it, "remove member failed")
                _work.value = GroupWork.Failed(it.reason("Could not remove them"))
            }
    }

    fun rename(value: String) = viewModelScope.launch {
        runCatching { CoreBridge.setConversationTitle(conversation, value.trim()) }
            .onFailure { Timber.tag(TAG).e(it, "rename failed") }
    }

    /** Leave, then step back to the home list — this chat can no longer send. */
    fun leave() = viewModelScope.launch {
        _work.value = GroupWork.Busy
        runCatching { CoreBridge.leaveGroup(conversation) }
            .onSuccess {
                _work.value = GroupWork.Idle
                navigator.reset(Routes.App)
            }
            .onFailure {
                Timber.tag(TAG).e(it, "leave failed")
                _work.value = GroupWork.Failed(it.reason("Could not leave"))
            }
    }

    fun clearError() { _work.value = GroupWork.Idle }

    private companion object {
        const val TAG = "GroupVM"
    }
}
