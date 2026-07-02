package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import androidx.lifecycle.ViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow

//enum class UsersGroupMode {
//    NONE,
//
//    /**
//     * "Anonymous" users at top, nicknamed users afterwards in alphabetically sorted groups
//     */
//    BY_FIRST_LETTER
//}

//typealias GroupedUserList = Flow<Map<String, List<User>>>

class SavedUsersVM(
    private val application: Application,
//    userRepository: UserRepository
) : ViewModel() {
    private val context: Context get() = application.applicationContext

    private val _searchQuery = MutableStateFlow("")
    val searchQuery = _searchQuery.asStateFlow()

    private val _users = MutableStateFlow(emptyMap<String, Any>())
    val users = _users.asStateFlow()
//        userRepository.fetchAll(_searchQuery.value)
//            .map { groupUsersFlow(it, UsersGroupMode.BY_FIRST_LETTER) }
//            .onEach { _isLoading.value = false }
//            .stateIn(viewModelScope, SharingStarted.WhileSubscribed(), emptyMap())


    private val _isLoading = MutableStateFlow(true)
    val isLoading = _isLoading.asStateFlow()

//    private fun groupUsersFlow(users: List<User>, mode: UsersGroupMode) = when (mode) {
//        UsersGroupMode.NONE -> {
//            mapOf("All" to users)
//        }
//
//        UsersGroupMode.BY_FIRST_LETTER -> {
//            users.groupBy { it.nickname.firstOrNull()?.uppercase() ?: "#" }
//        }
//    }

    init {
        _isLoading.value = false


    }
}