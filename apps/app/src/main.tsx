import React from "react";
import ReactDOM from "react-dom/client";
import { router, RouterProvider } from "./router";
import { ApolloProvider } from '@apollo/client';
import { apolloClient } from './lib/apollo';

import "./index.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ApolloProvider client={apolloClient}>
      <RouterProvider router={router} />
    </ApolloProvider>
  </React.StrictMode>,
);
