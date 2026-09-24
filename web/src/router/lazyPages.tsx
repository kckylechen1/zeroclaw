import { lazy } from 'react';

export const Dashboard = lazy(() => import('../pages/Dashboard'));
export const AgentChat = lazy(() => import('../pages/AgentChat'));
export const AgentsList = lazy(() => import('../pages/AgentsList'));
export const Config = lazy(() => import('../pages/Config'));
export const Cron = lazy(() => import('../pages/Cron'));
export const Logs = lazy(() => import('../pages/Logs'));
export const Pairing = lazy(() => import('../pages/Pairing'));
export const Skills = lazy(() => import('../pages/Skills'));
