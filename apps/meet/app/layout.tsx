import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "PulseBeam Meet",
  description: "A distraction-free video conferencing app",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body
        className={`antialiased dark`}
      >
        {children}
      </body>
    </html>
  );
}

