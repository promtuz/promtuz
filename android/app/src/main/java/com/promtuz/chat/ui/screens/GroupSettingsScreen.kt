package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.TextButton
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.CircularProgressIndicator
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
import com.promtuz.chat.ui.components.CardGap
import com.promtuz.chat.ui.components.SettingsCard
import com.promtuz.chat.ui.components.SettingsSwitchRow
import com.promtuz.chat.ui.components.GroupedScreen
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

    GroupedScreen({ Text("Group settings") }, snackbarHost = { SnackbarHost(snackbar) }) { padding ->
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
            return@GroupedScreen
        }
        val current = rules ?: return@GroupedScreen
        val set = { next: GroupRulesRecord -> if (!busy && next != current) viewModel.setRules(next) }
        Column(
            Modifier.fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(padding.listPadding()),
            verticalArrangement = Arrangement.Top,
        ) {
            SettingsCard(header = "Members can") {
                SettingsSwitchRow("Send messages", current.membersSend, { set(current.copy(membersSend = it)) },
                    subtitle = if (current.membersSend) "Everyone can send messages" else "Only admins can send messages",
                    enabled = !busy)
                SettingsSwitchRow("Edit group info", current.membersEdit, { set(current.copy(membersEdit = it)) },
                    subtitle = if (current.membersEdit) "Everyone can change the name and photo"
                        else "Only admins can change the name and photo",
                    enabled = !busy)
                SettingsSwitchRow("Add members", current.membersAdd, { set(current.copy(membersAdd = it)) },
                    subtitle = if (current.membersAdd) "Everyone can add people" else "Only admins can add people",
                    enabled = !busy)
            }

            if (role == 2) {
                CardGap()
                SettingsCard(header = "Admins") {
                    SettingsSwitchRow("Make admins", current.adminsAppoint, { set(current.copy(adminsAppoint = it)) },
                        subtitle = if (current.adminsAppoint) "Admins can make other members admins"
                            else "Only owners can make admins",
                        enabled = !busy)
                }
            }
        }
    }
}
