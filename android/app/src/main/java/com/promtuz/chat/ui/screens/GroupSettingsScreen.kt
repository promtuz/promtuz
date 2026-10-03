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
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.presentation.viewmodel.GroupVM
import com.promtuz.chat.presentation.viewmodel.GroupWork
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SettingsSection
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.listPadding
import org.koin.androidx.compose.koinViewModel
import uniffi.core.GroupRulesRecord

/** Admins change these rules; only owners decide whether admins appoint admins. */
@Composable
fun GroupSettingsScreen(conversationHex: String, viewModel: GroupVM = koinViewModel()) {
    val rules by viewModel.rules.collectAsStateWithLifecycle()
    val role by viewModel.role.collectAsStateWithLifecycle()
    val work by viewModel.work.collectAsStateWithLifecycle()
    val notice by viewModel.notice.collectAsStateWithLifecycle()
    val loading by viewModel.loading.collectAsStateWithLifecycle()
    val loadError by viewModel.loadError.collectAsStateWithLifecycle()
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
                .padding(padding.listPadding()),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            SettingsSection("Members can")
            GroupedActionRow(
                title = "Send messages",
                supportingText = if (current.membersSend) "Everyone can send messages" else "Only admins can send messages",
                index = 0, groupSize = 3, checked = current.membersSend,
                onClick = { set(current.copy(membersSend = !current.membersSend)) },
                control = { Switch(checked = current.membersSend, onCheckedChange = null, enabled = !busy) },
            )
            GroupedActionRow(
                title = "Edit group info",
                supportingText = if (current.membersEdit) "Everyone can change the name and photo"
                    else "Only admins can change the name and photo",
                index = 1, groupSize = 3, checked = current.membersEdit,
                onClick = { set(current.copy(membersEdit = !current.membersEdit)) },
                control = { Switch(checked = current.membersEdit, onCheckedChange = null, enabled = !busy) },
            )
            GroupedActionRow(
                title = "Add members",
                supportingText = if (current.membersAdd) "Everyone can add people" else "Only admins can add people",
                index = 2, groupSize = 3, checked = current.membersAdd,
                onClick = { set(current.copy(membersAdd = !current.membersAdd)) },
                control = { Switch(checked = current.membersAdd, onCheckedChange = null, enabled = !busy) },
            )

            if (role == 2) {
                SettingsSection("Admins")
                GroupedActionRow(
                    title = "Make admins",
                    supportingText = if (current.adminsAppoint) "Admins can make other members admins"
                        else "Only owners can make admins",
                    index = 0, groupSize = 1, checked = current.adminsAppoint,
                    onClick = { set(current.copy(adminsAppoint = !current.adminsAppoint)) },
                    control = { Switch(checked = current.adminsAppoint, onCheckedChange = null, enabled = !busy) },
                )
            }
        }
    }
}
