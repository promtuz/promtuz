package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.TextButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.Alignment
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.presentation.viewmodel.GroupVM
import com.promtuz.chat.presentation.viewmodel.GroupWork
import com.promtuz.chat.ui.components.SimpleScreen
import org.koin.androidx.compose.koinViewModel
import uniffi.core.GroupRulesRecord

/** What a group's members may do. Admins change it; whether admins appoint admins is for owners. */
@Composable
fun GroupSettingsScreen(conversationHex: String, viewModel: GroupVM = koinViewModel()) {
    val rules by viewModel.rules.collectAsStateWithLifecycle()
    val role by viewModel.role.collectAsStateWithLifecycle()
    val work by viewModel.work.collectAsStateWithLifecycle()
    val notice by viewModel.notice.collectAsStateWithLifecycle()
    val loading by viewModel.loading.collectAsStateWithLifecycle()
    val loadError by viewModel.loadError.collectAsStateWithLifecycle()
    val direction = LocalLayoutDirection.current
    val snackbar = remember { SnackbarHostState() }
    LaunchedEffect(conversationHex) { viewModel.load(conversationHex) }
    LaunchedEffect(notice) { notice?.let { snackbar.showSnackbar(it); viewModel.clearNotice() } }
    LaunchedEffect(work) { (work as? GroupWork.Failed)?.let { snackbar.showSnackbar(it.reason); viewModel.clearError() } }
    val busy = work is GroupWork.Busy

    SimpleScreen({ Text("Group settings") }, snackbarHost = { SnackbarHost(snackbar) }) { padding ->
        if (loading || loadError || rules == null || role < 1) {
            Column(Modifier.fillMaxWidth().padding(padding).padding(24.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                when {
                    loading -> CircularProgressIndicator()
                    loadError -> {
                        Text("Couldn’t load the group")
                        TextButton(onClick = { viewModel.load(conversationHex) }) { Text("Retry") }
                    }
                    else -> Text("Group settings aren’t available")
                }
            }
            return@SimpleScreen
        }
        val current = rules ?: return@SimpleScreen
        val set = { next: GroupRulesRecord -> if (!busy && next != current) viewModel.setRules(next) }
        Column(
            Modifier.fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(
                    start = padding.calculateLeftPadding(direction) + 18.dp,
                    end = padding.calculateRightPadding(direction) + 18.dp,
                    top = padding.calculateTopPadding() + 12.dp,
                    bottom = padding.calculateBottomPadding() + 24.dp,
                ),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            SettingsSection("Members can")
            SettingsRow(
                title = "Send messages",
                detail = if (current.membersSend) "Everyone can send messages" else "Only admins can send messages",
                index = 0, count = 3, checked = current.membersSend,
                onClick = { set(current.copy(membersSend = !current.membersSend)) },
            ) { Switch(checked = current.membersSend, onCheckedChange = null, enabled = !busy) }
            SettingsRow(
                title = "Edit group info",
                detail = if (current.membersEdit) "Everyone can change the name and photo"
                    else "Only admins can change the name and photo",
                index = 1, count = 3, checked = current.membersEdit,
                onClick = { set(current.copy(membersEdit = !current.membersEdit)) },
            ) { Switch(checked = current.membersEdit, onCheckedChange = null, enabled = !busy) }
            SettingsRow(
                title = "Add members",
                detail = if (current.membersAdd) "Everyone can add people" else "Only admins can add people",
                index = 2, count = 3, checked = current.membersAdd,
                onClick = { set(current.copy(membersAdd = !current.membersAdd)) },
            ) { Switch(checked = current.membersAdd, onCheckedChange = null, enabled = !busy) }

            if (role == 2) {
                SettingsSection("Admins")
                SettingsRow(
                    title = "Make admins",
                    detail = if (current.adminsAppoint) "Admins can make other members admins"
                        else "Only owners can make admins",
                    checked = current.adminsAppoint,
                    onClick = { set(current.copy(adminsAppoint = !current.adminsAppoint)) },
                ) { Switch(checked = current.adminsAppoint, onCheckedChange = null, enabled = !busy) }
            }
        }
    }
}
