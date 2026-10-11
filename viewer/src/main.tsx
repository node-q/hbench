import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter, RouterProvider } from "react-router";
import "@fontsource/space-grotesk/latin-500.css";
import "@fontsource/space-grotesk/latin-600.css";
import "@fontsource/ibm-plex-sans/latin-400.css";
import "@fontsource/ibm-plex-sans/latin-500.css";
import "@fontsource/ibm-plex-sans/latin-600.css";
import "@fontsource/ibm-plex-mono/latin-400.css";
import "@fontsource/ibm-plex-mono/latin-500.css";
import "./tokens.css";
import "./base.css";
import { Shell, NotFound } from "./shell";
import Home from "./pages/home/Home";
import Job from "./pages/job/Job";
import Trial from "./pages/trial/Trial";
import Compare from "./pages/compare/Compare";
import Run from "./pages/run/Run";
import TaskDefinitions from "./pages/tasks/TaskDefinitions";
import TaskDefinition from "./pages/tasks/TaskDefinition";

const router = createBrowserRouter([
  {
    element: <Shell />,
    children: [
      { index: true, element: <Home /> },
      { path: "jobs/:jobName", element: <Job /> },
      { path: "jobs/:jobName/trials/:trialName", element: <Trial /> },
      { path: "compare", element: <Compare /> },
      { path: "run", element: <Run /> },
      { path: "task-definitions", element: <TaskDefinitions /> },
      { path: "task-definitions/:taskName", element: <TaskDefinition /> },
      { path: "*", element: <NotFound /> },
    ],
  },
]);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <RouterProvider router={router} />
  </StrictMode>,
);
