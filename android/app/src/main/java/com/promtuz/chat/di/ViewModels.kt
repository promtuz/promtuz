package com.promtuz.chat.di

import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.presentation.viewmodel.NetworkStatsVM
import com.promtuz.chat.presentation.viewmodel.QrScannerVM
import com.promtuz.chat.presentation.viewmodel.SavedUsersVM
import com.promtuz.chat.presentation.viewmodel.SettingsVM
import com.promtuz.chat.presentation.viewmodel.ShareIdentityVM
import com.promtuz.chat.presentation.viewmodel.WelcomeVM
import org.koin.core.module.dsl.singleOf
import org.koin.core.module.dsl.viewModelOf
import org.koin.dsl.module

val vmModule = module {
    singleOf(::AppVM)

    viewModelOf(::WelcomeVM)
    viewModelOf(::ShareIdentityVM)
    viewModelOf(::QrScannerVM)
    viewModelOf(::SavedUsersVM)
    viewModelOf(::SettingsVM)
    viewModelOf(::ChatVM)
    viewModelOf(::NetworkStatsVM)
}